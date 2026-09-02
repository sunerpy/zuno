//! Durable persistence of the verification receipts tools report.
//!
//! A tool that ran a check knows whether the check passed and how much authority its
//! exit status carries; nothing else in the process does. It states that in
//! [`zuno_tool::VerificationReceipt`], attached to its own output. This module is the
//! only place that turns such a statement into a durable row, and the only place that
//! hands the model an identifier it can cite later.
//!
//! It also expires evidence, which is the same job seen from the other side. A check
//! proves something about the files as they were when it ran, so a call that writes a
//! file invalidates every earlier proof. Recording the proof and retiring it belong
//! together: split apart, the two could disagree about the order of the same call.
//!
//! # Why a hook and not the turn loop
//!
//! [`zuno_engine::hooks::ToolHooks::after`] is the one seam that sees every completed
//! tool call with its output still mutable, which is what lets the assigned receipt id
//! be written back where the model will read it. Doing the same work in the turn loop
//! would mean recording after the output had already been rendered, so the model would
//! be told a check passed without being told how to cite it — and an uncitable receipt
//! is indistinguishable from an absent one to the goal completion gate.
//!
//! # Why the id is written into the output text
//!
//! A tool's `metadata` is host-only. The provider request carries a tool result whose
//! content is the output string and nothing else, so an id published only under
//! [`zuno_tool::VERIFICATION_METADATA_KEY`] is invisible to the model that has to cite
//! it. The id therefore goes in both places: the metadata for hosts and the transcript,
//! and one appended line for the model. That line also states whether the receipt may
//! be cited at all, because a passing exit status from a pipeline that swallows failure
//! is recorded but is not proof, and the distinction is worthless if only the host can
//! see it.
//!
//! # Why a ledger failure is not a tool failure
//!
//! Returning an error from the `after` hook converts a successful result into a failed
//! one. That is the wrong trade here. By the time a receipt exists the tool has already
//! run: the command executed, the file was written. Reporting that as a failure tells
//! the model a mutation did not happen when it did, which is a worse lie than a missing
//! record and invites a destructive retry.
//!
//! So a ledger failure degrades the *claim* instead of the result. The receipt is
//! rewritten as [`zuno_tool::ReceiptOutcome::Unknown`] with the reason, and no receipt
//! id is published. The tool still reports what it did, and nothing downstream can
//! offer the call as proof, because
//! [`zuno_db::verification::VerificationReceipt::proves_success`] is false for an
//! unknown outcome and a citation the ledger never stored resolves to nothing.

use std::sync::Arc;

use sha2::{Digest as _, Sha256};
use zuno_engine::hooks::ToolHooks;
use zuno_tool::{
    ExitAuthority, ReceiptOutcome, ToolOutput, VERIFICATION_METADATA_KEY, VerificationReceipt,
};

/// Prefix on every receipt id, so a citation is recognizable in prose.
const RECEIPT_ID_PREFIX: &str = "rcp_";

/// Hex characters of the digest kept in a receipt id.
///
/// Sixteen, because the id is meant to be copied by a language model out of one tool
/// result and into the arguments of another. A full 64-character digest invites a
/// transcription error that would be indistinguishable from a fabricated citation,
/// and 16 hex characters is 64 bits — collision-free across any realistic session.
const RECEIPT_ID_DIGEST_CHARS: usize = 16;

/// The key under which the assigned id is published back to the model.
const RECEIPT_ID_FIELD: &str = "receiptId";

/// The receipt id for one tool call, derived rather than generated.
///
/// Derived from the session and the call so that a replayed call resolves to the same
/// row and the same citation. A random id would make the ledger's
/// `(session_id, tool_call_id)` upsert rewrite the identifier a model had already been
/// given, turning a correct citation into a dangling one. The session is part of the
/// input because the id is a global primary key while a provider's call ids are only
/// unique within a conversation.
#[must_use]
pub(crate) fn receipt_id(session_id: &str, call_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(call_id.as_bytes());
    let digest = hasher.finalize();
    let mut id = String::with_capacity(RECEIPT_ID_PREFIX.len() + RECEIPT_ID_DIGEST_CHARS);
    id.push_str(RECEIPT_ID_PREFIX);
    for byte in digest.iter().take(RECEIPT_ID_DIGEST_CHARS / 2) {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// Records tool-reported receipts, publishes the id that cites them, and retires the
/// evidence a write invalidated.
pub(crate) struct VerificationLedger {
    database: Arc<zuno_db::pool::Pool>,
    goals: Arc<zuno_goal::GoalStore>,
}

impl VerificationLedger {
    /// Wrap the session database and goal store this host already opened.
    pub(crate) const fn new(
        database: Arc<zuno_db::pool::Pool>,
        goals: Arc<zuno_goal::GoalStore>,
    ) -> Self {
        Self { database, goals }
    }

    /// Report a write, and tell the model what proof it just invalidated.
    ///
    /// Driven by the paths the tool itself reported, not by a list of writing tool ids:
    /// [`zuno_tool::METADATA_WRITTEN_PATHS_KEY`] documents why, and a goal that silently
    /// stopped being gated because a list was never updated is the failure this whole
    /// mechanism exists to prevent.
    ///
    /// Both calls are no-ops for a session with no goal, so no guard query is needed.
    /// Neither failure is fatal: the write already happened, and refusing the call would
    /// report a completed edit as a failure. A note says so instead, because a run told
    /// nothing would go on citing a check that no longer describes the files.
    fn report_write(&self, session_id: &str, tool: &str, output: &mut ToolOutput) {
        let written = output.written_paths();
        if written.is_empty() {
            return;
        }
        let reason = match written.as_slice() {
            [only] => format!("`{tool}` wrote {only}"),
            [first, rest @ ..] => format!(
                "`{tool}` wrote {first} and {count} more",
                count = rest.len()
            ),
            [] => unreachable!("checked above"),
        };
        let at_ms = zuno_db::message::now_millis();
        if let Err(error) = self.goals.escalate_to_change(session_id, &reason, at_ms) {
            announce(
                output,
                "[goal evidence unavailable]",
                &format!(
                    "this change could not be recorded against the goal ({error}), so                      completion may not be gated on evidence."
                ),
            );
            return;
        }
        match self.goals.mark_mutation(session_id, at_ms) {
            Ok(0) => {}
            Ok(reopened) => announce(
                output,
                "[goal evidence]",
                &format!(
                    "{reopened} satisfied criteri{a} went back to open, because this change                      came after the check that satisfied {it}. Verify again after your last                      edit and cite the new receipt.",
                    a = if reopened == 1 { "on" } else { "a" },
                    it = if reopened == 1 { "it" } else { "them" },
                ),
            ),
            Err(error) => announce(
                output,
                "[goal evidence unavailable]",
                &format!(
                    "evidence recorded before this change could not be retired ({error});                      treat any earlier check as describing files that have since changed."
                ),
            ),
        }
    }
}

/// Translate the tool-side authority into the stored one.
///
/// Two enums rather than one because `zuno-db` and `zuno-tool` do not depend on each
/// other: a tool states its authority without a database in scope, and the ledger
/// stores it without a tool trait in scope. This crate depends on both, so the
/// translation belongs here and stays exhaustive on purpose — a new authority level
/// must not silently become an existing one.
const fn stored_authority(
    authority: zuno_tool::ExitAuthority,
) -> zuno_db::verification::ExitAuthority {
    match authority {
        zuno_tool::ExitAuthority::Authoritative => {
            zuno_db::verification::ExitAuthority::Authoritative
        }
        zuno_tool::ExitAuthority::Derived => zuno_db::verification::ExitAuthority::Derived,
        zuno_tool::ExitAuthority::Absent => zuno_db::verification::ExitAuthority::Absent,
    }
}

/// Translate the tool-side outcome into the stored one.
const fn stored_outcome(
    outcome: zuno_tool::ReceiptOutcome,
) -> zuno_db::verification::ReceiptOutcome {
    match outcome {
        zuno_tool::ReceiptOutcome::Passed => zuno_db::verification::ReceiptOutcome::Passed,
        zuno_tool::ReceiptOutcome::Failed => zuno_db::verification::ReceiptOutcome::Failed,
        zuno_tool::ReceiptOutcome::Unknown => zuno_db::verification::ReceiptOutcome::Unknown,
    }
}

/// The tag that opens the appended line, so a citation is findable in a transcript.
const RECEIPT_NOTE_TAG: &str = "verification";

/// What the model is told when the receipt is usable as proof.
const CITABLE: &str = "Cite this id as evidence that the check passed.";

/// What the model is told when it is not.
const NOT_CITABLE: &str = "Recorded, but this proves nothing and cannot be cited as evidence.";

/// How much the exit status is worth, in the words the model reads.
const fn describe_authority(authority: ExitAuthority) -> &'static str {
    match authority {
        ExitAuthority::Authoritative => "authoritative",
        ExitAuthority::Derived => "derived, so it may not reflect every stage",
        ExitAuthority::Absent => "no exit status",
    }
}

/// What the call decided, in the words the model reads.
const fn describe_outcome(outcome: ReceiptOutcome) -> &'static str {
    match outcome {
        ReceiptOutcome::Passed => "passed",
        ReceiptOutcome::Failed => "failed",
        ReceiptOutcome::Unknown => "undecided",
    }
}

/// Append one line to the text the model will read.
///
/// Appended rather than prepended so the tool's own output still leads: the line is a
/// footnote about the call, not a replacement for what the call said.
fn announce(output: &mut ToolOutput, headline: &str, verdict: &str) {
    if !output.output.is_empty() {
        output.output.push_str("\n\n");
    }
    output.output.push_str(headline);
    output.output.push(' ');
    output.output.push_str(verdict);
}

/// Announce a stored receipt and say whether it may be cited.
fn announce_stored(output: &mut ToolOutput, id: &str, receipt: &VerificationReceipt) {
    let authority = describe_authority(receipt.exit_authority);
    let status = match receipt.exit_code {
        Some(code) => format!("exit {code}, {authority}"),
        None => authority.to_owned(),
    };
    let headline = format!(
        "[{RECEIPT_NOTE_TAG} {id}] {outcome}: {summary} ({status}).",
        outcome = describe_outcome(receipt.outcome),
        summary = receipt.summary,
    );
    let verdict = if receipt.proves_success() {
        CITABLE
    } else {
        NOT_CITABLE
    };
    announce(output, &headline, verdict);
}

/// Replace a published receipt with one that proves nothing, and say why.
///
/// The summary survives so the tool result still describes what ran. No id appears in
/// the metadata or in the appended line, because there is no stored row to cite, and a
/// model told to cite a row that does not exist would produce a dangling citation that
/// the completion gate cannot tell from a fabricated one.
fn degrade(output: &mut ToolOutput, summary: String, reason: String) {
    let receipt = VerificationReceipt::unknown(summary, reason);
    let headline = format!(
        "[{RECEIPT_NOTE_TAG} unavailable] {summary}: {reason}.",
        summary = receipt.summary,
        reason = receipt.detail.as_deref().unwrap_or("no reason recorded"),
    );
    output.metadata.insert(
        VERIFICATION_METADATA_KEY.to_owned(),
        receipt.to_metadata_value(),
    );
    announce(
        output,
        &headline,
        "No receipt was recorded, so this cannot be cited as evidence.",
    );
}

#[async_trait::async_trait]
impl ToolHooks for VerificationLedger {
    /// Persist the receipt this call reported, if it reported one.
    ///
    /// # Errors
    ///
    /// Never. Every failure mode degrades the receipt in place instead, for the reason
    /// given in the module documentation: the tool has already run, and an error here
    /// would misreport a side effect that really happened.
    async fn after(
        &self,
        tool: &str,
        session_id: &str,
        call_id: &str,
        _args: &serde_json::Value,
        output: &mut ToolOutput,
    ) -> Result<(), String> {
        self.report_write(session_id, tool, output);
        let reported = match VerificationReceipt::from_metadata(&output.metadata) {
            Ok(None) => return Ok(()),
            Ok(Some(receipt)) => receipt,
            Err(error) => {
                degrade(
                    output,
                    format!("{tool} reported a verification receipt that could not be read"),
                    format!("the receipt metadata was malformed: {error}"),
                );
                return Ok(());
            }
        };

        let id = receipt_id(session_id, call_id);
        let record = zuno_db::verification::NewVerificationReceipt {
            id: id.clone(),
            session_id: session_id.to_owned(),
            // The dispatch seam carries no turn identity. The session and the call are
            // what evidence resolution matches on, and adding a turn id would mean
            // widening a trait the engine's own tests implement.
            turn_id: None,
            tool_call_id: call_id.to_owned(),
            tool_id: tool.to_owned(),
            summary: reported.summary.clone(),
            workdir: reported.workdir.clone(),
            exit_code: reported.exit_code,
            exit_authority: stored_authority(reported.exit_authority),
            outcome: stored_outcome(reported.outcome),
            git_head: reported.git_head.clone(),
            output_digest: reported.output_digest.clone(),
            detail: reported.detail.clone(),
            time_created: zuno_db::message::now_millis(),
        };

        let database = Arc::clone(&self.database);
        let stored = tokio::task::spawn_blocking(move || {
            let connection = database.get()?;
            zuno_db::verification::record(&connection, &record)
        })
        .await;

        match stored {
            Ok(Ok(())) => {
                if let Some(serde_json::Value::Object(published)) =
                    output.metadata.get_mut(VERIFICATION_METADATA_KEY)
                {
                    published.insert(
                        RECEIPT_ID_FIELD.to_owned(),
                        serde_json::Value::String(id.clone()),
                    );
                }
                announce_stored(output, &id, &reported);
                Ok(())
            }
            Ok(Err(error)) => {
                degrade(
                    output,
                    reported.summary,
                    format!("the verification ledger rejected the receipt: {error}"),
                );
                Ok(())
            }
            Err(error) => {
                degrade(
                    output,
                    reported.summary,
                    format!("the verification ledger write did not complete: {error}"),
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_tool::{ExitAuthority, ReceiptOutcome};

    fn pool() -> Arc<zuno_db::pool::Pool> {
        let pool = zuno_db::pool::Pool::open(&zuno_paths::DbLocation::Memory)
            .expect("in-memory session database");
        {
            let mut connection = pool.get().expect("database connection");
            zuno_db::migration::apply(&mut connection).expect("current schema");
        }
        Arc::new(pool)
    }

    /// A ledger over one in-memory database, with the goal tables attached.
    fn evidence_ledger(
        database: &Arc<zuno_db::pool::Pool>,
    ) -> (VerificationLedger, tempfile::TempDir) {
        let spill = tempfile::tempdir().expect("spill directory");
        let goals = Arc::new(
            zuno_goal::GoalStore::from_pool(Arc::clone(database), spill.path().to_owned())
                .expect("attach the goal tables"),
        );
        (VerificationLedger::new(Arc::clone(database), goals), spill)
    }

    fn output_with(receipt: &VerificationReceipt) -> ToolOutput {
        ToolOutput::text("shell", "ran").with_verification(receipt)
    }

    #[tokio::test]
    async fn a_tool_that_reports_nothing_writes_no_receipt() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);
        let mut output = ToolOutput::text("shell", "ran");

        ledger
            .after(
                "read",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("a silent tool is not an error");

        let connection = database.get().expect("connection");
        assert!(
            zuno_db::verification::for_session(&connection, "ses_1")
                .expect("query")
                .is_empty(),
            "a tool that claimed nothing must leave no evidence behind"
        );
    }

    #[tokio::test]
    async fn a_reported_receipt_is_stored_and_its_id_is_published_to_the_model() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);
        let mut output = output_with(&VerificationReceipt::passed("cargo test -p zuno-db"));

        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("recording a receipt");

        let published = output
            .metadata
            .get(VERIFICATION_METADATA_KEY)
            .and_then(|value| value.get(RECEIPT_ID_FIELD))
            .and_then(serde_json::Value::as_str)
            .expect("the model must be given an id it can cite")
            .to_owned();
        assert_eq!(published, receipt_id("ses_1", "call_1"));

        let connection = database.get().expect("connection");
        let stored = zuno_db::verification::find(&connection, "ses_1", &published)
            .expect("query")
            .expect("the published id must resolve to a stored receipt");
        assert_eq!(stored.tool_id, "shell");
        assert_eq!(stored.tool_call_id, "call_1");
        assert_eq!(stored.summary, "cargo test -p zuno-db");
        assert!(
            stored.proves_success(),
            "a passing authoritative check must be usable as evidence"
        );
    }

    #[tokio::test]
    async fn a_replayed_call_keeps_one_receipt_and_one_citable_id() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);

        let mut first = output_with(&VerificationReceipt::passed("cargo test"));
        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut first,
            )
            .await
            .expect("first record");
        let mut second = output_with(&VerificationReceipt::failed("cargo test", Some(101)));
        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut second,
            )
            .await
            .expect("second record");

        let connection = database.get().expect("connection");
        let receipts = zuno_db::verification::for_session(&connection, "ses_1").expect("query");
        assert_eq!(receipts.len(), 1, "a replay must not fork the evidence");
        assert_eq!(
            receipts[0].id,
            receipt_id("ses_1", "call_1"),
            "the id a model was already given must survive the replay"
        );
        assert!(
            !receipts[0].proves_success(),
            "the later, failing observation is the one that counts"
        );
    }

    /// The provider request carries the output string, not the metadata.
    ///
    /// This is the whole reason the id is appended to the text: a receipt the model
    /// cannot read is a receipt it cannot cite, and an uncitable receipt fails the
    /// completion gate exactly like a missing one.
    #[tokio::test]
    async fn the_receipt_id_reaches_the_model_in_the_text_and_not_only_the_metadata() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);
        let mut output = output_with(&VerificationReceipt::passed("cargo test -p zuno-db"));

        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("recording a receipt");

        let id = receipt_id("ses_1", "call_1");
        assert!(
            output.output.contains(&id),
            "the model reads only the output string: {}",
            output.output
        );
        assert!(output.output.contains(CITABLE), "{}", output.output);
        assert!(
            output.output.starts_with("ran"),
            "the tool's own output must still lead: {}",
            output.output
        );
    }

    #[tokio::test]
    async fn a_status_that_proves_nothing_says_so_where_the_model_reads_it() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);
        let mut receipt = VerificationReceipt::passed("cargo test | tail -5");
        receipt.exit_authority = ExitAuthority::Derived;
        let mut output = output_with(&receipt);

        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("recording a receipt");

        assert!(output.output.contains(NOT_CITABLE), "{}", output.output);
        assert!(
            !output.output.contains(CITABLE),
            "a derived status must never invite a citation: {}",
            output.output
        );
        assert!(
            output.output.contains("derived"),
            "the model is told why: {}",
            output.output
        );
    }

    #[tokio::test]
    async fn a_degraded_receipt_offers_the_model_no_id_at_all() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);
        let mut output = ToolOutput::text("shell", "ran");
        output.metadata.insert(
            VERIFICATION_METADATA_KEY.to_owned(),
            serde_json::json!({ "outcome": 7 }),
        );

        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("a malformed claim is not a failed call");

        assert!(
            !output.output.contains(RECEIPT_ID_PREFIX),
            "there is no stored row, so the model must be offered nothing to cite: {}",
            output.output
        );
        assert!(
            output.output.contains("cannot be cited as evidence"),
            "{}",
            output.output
        );
    }

    #[tokio::test]
    async fn a_derived_exit_status_is_stored_but_proves_nothing() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);
        let mut receipt = VerificationReceipt::passed("cargo test | tail -5");
        receipt.exit_authority = ExitAuthority::Derived;
        let mut output = output_with(&receipt);

        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("recording a receipt");

        let connection = database.get().expect("connection");
        let stored = zuno_db::verification::for_session(&connection, "ses_1").expect("query");
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].outcome,
            zuno_db::verification::ReceiptOutcome::Passed
        );
        assert!(
            !stored[0].proves_success(),
            "a zero exit code from a pipeline that does not propagate failure is not proof"
        );
    }

    #[tokio::test]
    async fn malformed_receipt_metadata_degrades_the_claim_without_failing_the_call() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);
        let mut output = ToolOutput::text("shell", "ran");
        output.metadata.insert(
            VERIFICATION_METADATA_KEY.to_owned(),
            serde_json::json!({ "outcome": 7 }),
        );

        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("a malformed claim must not turn a completed call into a failure");

        let republished = VerificationReceipt::from_metadata(&output.metadata)
            .expect("the degraded receipt must be readable")
            .expect("the degraded receipt must be present");
        assert_eq!(republished.outcome, ReceiptOutcome::Unknown);
        assert!(
            !republished.proves_success(),
            "a receipt nobody could parse must never be evidence"
        );
        assert!(
            output
                .metadata
                .get(VERIFICATION_METADATA_KEY)
                .and_then(|value| value.get(RECEIPT_ID_FIELD))
                .is_none(),
            "there is no stored row, so there must be no id to cite"
        );
    }

    /// The whole chain, across three crates, in one test.
    ///
    /// A tool reports a check; the ledger stores it and hands the model an id; the goal
    /// gate accepts that id as evidence; a later write retires the evidence and says so.
    /// Each half is unit-tested elsewhere, but only this proves the halves agree about
    /// the identifier and the clock — and an id that resolves to nothing is exactly as
    /// useless as no id at all.
    #[tokio::test]
    async fn a_recorded_check_satisfies_a_criterion_until_a_later_write_retires_it() {
        let database = pool();
        let spill = tempfile::tempdir().expect("spill directory");
        let goals = Arc::new(
            zuno_goal::GoalStore::from_pool(Arc::clone(&database), spill.path().to_owned())
                .expect("attach the goal tables"),
        );
        let ledger = VerificationLedger::new(Arc::clone(&database), Arc::clone(&goals));
        let created = goals
            .create_goal_with_criteria(
                "ses_1",
                "make the tests pass",
                &["cargo test -p zuno-db succeeds".to_owned()],
                None,
            )
            .expect("create a goal with one criterion");
        let criterion = created.criteria[0].criterion_id.clone();

        let mut checked = output_with(&VerificationReceipt::passed("cargo test -p zuno-db"));
        ledger
            .after(
                "shell",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut checked,
            )
            .await
            .expect("record the check");
        let cited = checked
            .metadata
            .get(VERIFICATION_METADATA_KEY)
            .and_then(|value| value.get(RECEIPT_ID_FIELD))
            .and_then(serde_json::Value::as_str)
            .expect("an id to cite")
            .to_owned();

        // Satisfied at the epoch, so the write below is unambiguously later than the
        // proof: both sides stamp in milliseconds, and a tie would not reopen.
        goals
            .satisfy_criterion("ses_1", created.goal.revision, &criterion, &cited, 1)
            .expect("the goal gate must accept the id the ledger published");

        let mut wrote = ToolOutput::text("edit", "wrote 1 file")
            .with_written_path(std::path::Path::new("src/main.rs"));
        ledger
            .after(
                "edit",
                "ses_1",
                "call_2",
                &serde_json::Value::Null,
                &mut wrote,
            )
            .await
            .expect("a write is not a failure");

        assert_eq!(
            goals.kind("ses_1").expect("read the kind"),
            zuno_goal::GoalKind::Change,
            "a goal that wrote a file is no longer a question"
        );
        let after = goals.criteria("ses_1").expect("read the criteria");
        assert_eq!(after[0].status, zuno_goal::GoalCriterionStatus::Open);
        assert!(
            after[0].receipt_id.is_none(),
            "the retired citation must not survive the change it predates"
        );
        assert!(
            wrote.output.contains("went back to open"),
            "the model must be told its proof expired: {}",
            wrote.output
        );
        assert!(
            goals
                .satisfy_criterion(
                    "ses_1",
                    goals.goal("ses_1").expect("read").expect("a goal").revision,
                    &criterion,
                    &cited,
                    2,
                )
                .is_err(),
            "the same check must not satisfy the criterion again after the write"
        );
    }

    #[tokio::test]
    async fn a_call_that_writes_nothing_leaves_the_goal_a_question() {
        let database = pool();
        let spill = tempfile::tempdir().expect("spill directory");
        let goals = Arc::new(
            zuno_goal::GoalStore::from_pool(Arc::clone(&database), spill.path().to_owned())
                .expect("attach the goal tables"),
        );
        let ledger = VerificationLedger::new(Arc::clone(&database), Arc::clone(&goals));
        goals
            .create_goal_with_criteria("ses_1", "explain the loop", &[], None)
            .expect("create a goal");

        let mut output = ToolOutput::text("read", "file contents");
        ledger
            .after(
                "read",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("a read is not a mutation");

        assert_eq!(
            goals.kind("ses_1").expect("read the kind"),
            zuno_goal::GoalKind::Question,
            "reading files must not gate a goal that changes nothing"
        );
        assert_eq!(output.output, "file contents", "{}", output.output);
    }

    /// A tool may write outside any goal, and must not be punished for it.
    #[tokio::test]
    async fn a_write_in_a_session_with_no_goal_is_silent() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);
        let mut output = ToolOutput::text("edit", "wrote 1 file")
            .with_written_path(std::path::Path::new("src/main.rs"));

        ledger
            .after(
                "edit",
                "ses_1",
                "call_1",
                &serde_json::Value::Null,
                &mut output,
            )
            .await
            .expect("no goal is not an error");

        assert_eq!(output.output, "wrote 1 file", "{}", output.output);
    }

    #[test]
    fn a_receipt_id_is_stable_per_call_and_distinct_across_sessions() {
        assert_eq!(receipt_id("ses_1", "call_1"), receipt_id("ses_1", "call_1"));
        assert_ne!(receipt_id("ses_1", "call_1"), receipt_id("ses_2", "call_1"));
        assert_ne!(receipt_id("ses_1", "call_1"), receipt_id("ses_1", "call_2"));
        let id = receipt_id("ses_1", "call_1");
        assert!(id.starts_with(RECEIPT_ID_PREFIX), "{id}");
        assert_eq!(id.len(), RECEIPT_ID_PREFIX.len() + RECEIPT_ID_DIGEST_CHARS);
    }

    /// A provider that reuses call ids across sessions must not collide.
    ///
    /// The id is a global primary key while a call id is only unique within one
    /// conversation, which is why the session is hashed in. A test provider numbering
    /// its calls from one would otherwise fail the second session's first tool call.
    #[tokio::test]
    async fn two_sessions_may_reuse_the_same_call_id() {
        let database = pool();
        let (ledger, _spill) = evidence_ledger(&database);

        for session in ["ses_1", "ses_2"] {
            let mut output = output_with(&VerificationReceipt::passed("cargo test"));
            ledger
                .after(
                    "shell",
                    session,
                    "call_1",
                    &serde_json::Value::Null,
                    &mut output,
                )
                .await
                .unwrap_or_else(|error| panic!("recording for {session}: {error}"));
        }

        let connection = database.get().expect("connection");
        assert_eq!(
            zuno_db::verification::for_session(&connection, "ses_1")
                .expect("query")
                .len(),
            1
        );
        assert_eq!(
            zuno_db::verification::for_session(&connection, "ses_2")
                .expect("query")
                .len(),
            1
        );
    }
}

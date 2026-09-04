//! One durable settlement rule for every human request an ACP surface presents.
//!
//! Both askers in this crate — [`crate::AcpPermissionAsker`] and
//! [`crate::AcpQuestionAsker`], on both their live and their recovery paths — settle
//! their durable rows through [`settle`]. The rule lives here rather than once per
//! asker because the half of it that matters is invisible from the arm that gets it
//! wrong: three separate reviews found a decided dialog that resolved its durable
//! row and left the active Goal paused forever, once per surface.

use serde_json::Value;
use zuno_db::human_request::{HumanRequest, HumanRequestState, HumanRequestStore};
use zuno_goal::GoalStore;

/// Whatever a settlement failed on, for the caller to label with its own tool.
pub(crate) type SettleError = Box<dyn std::error::Error + Send + Sync>;

/// Whether a settled request must also produce model-visible durable input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settlement {
    /// The live tool call reports the outcome to the model itself.
    ResolveOnly,
    /// The originating call is gone, so the reply becomes durable inbox input.
    DurableInput,
}

/// Settle one presented dialog and lift the Goal pause its request created.
///
/// `response` is the caller's whole record of what happened, and it is the only
/// thing a caller may vary. Every terminal outcome — answered, denied, withdrawn,
/// expired, unrecognized, or undelivered — resolves to
/// [`HumanRequestState::Answered`], because [`GoalStore::resume_for_work`] lifts a
/// `waits_for_human_request` pause only while its request is exactly `Answered`, and
/// nothing else clears that pause row. Recording any other terminal state parks the
/// active Goal for the rest of the session on the most ordinary user action there
/// is, and Zuno's rule is that an active goal continues until it completes, is
/// deliberately paused or blocked, exhausts its budget, or fails permanently — a
/// dismissed dialog is none of those. The honesty of the record therefore lives in
/// `response`, which says how the reply was reached, not in the state column.
///
/// A dialog that never reached the client is not a settlement at all. Recovery
/// callers return without calling this, so that row stays pending and answerable by
/// the TUI, the HTTP broker, or the next attempt, and its Goal stays resumable
/// through the answer it is still waiting for.
///
/// The resume is keyed on the settled row's own `session_id`, the same row the
/// `goal_id` decision is read from. The pause row, the Goal, and the inbox admission
/// are all keyed on that column, so deciding from the row and resuming a value the
/// caller passed in would re-open the park for any writer that ever disagreed with
/// the payload it copied.
///
/// No parameter can opt out of the resume: there is no state argument and no
/// per-outcome flag, so a new outcome variant on either asker can only change
/// `response` and cannot grow an arm that forgets the Goal.
///
/// `Some` is the settled row. Under [`Settlement::DurableInput`] it means exactly
/// "this call settled it", because `answer_with_input` reports `None` for a row
/// another surface already answered. Under [`Settlement::ResolveOnly`] the guarded
/// `UPDATE` reads the row back either way, so `Some` means only that the row exists;
/// that path reports the outcome to the model through its own tool result and must
/// not branch on this value.
pub(crate) fn settle(
    store: &HumanRequestStore,
    goals: &GoalStore,
    request_id: &str,
    response: Value,
    settlement: Settlement,
) -> Result<Option<HumanRequest>, SettleError> {
    let now = zuno_db::message::now_millis();
    let resolved = match settlement {
        Settlement::ResolveOnly => store.resolve(
            request_id,
            HumanRequestState::Answered,
            Some(&response),
            now,
        )?,
        Settlement::DurableInput => store
            .answer_with_input(request_id, response, now)?
            .map(|(request, _input)| request),
    };
    let Some(resolved) = resolved else {
        return Ok(None);
    };
    if resolved.goal_id.is_some() {
        goals.resume_for_work(&resolved.session_id)?;
    }
    Ok(Some(resolved))
}

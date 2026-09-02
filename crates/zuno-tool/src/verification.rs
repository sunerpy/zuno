//! The receipt a tool attaches when its own result is verification evidence.
//!
//! A tool that runs a check knows things the transcript cannot recover later:
//! whether the exit status it reports covers the whole command, which directory
//! it ran in, and which repository revision it observed. Attaching that as a
//! typed receipt lets the host store it durably, so a later claim of success can
//! be checked against recorded evidence instead of against narration.
//!
//! Producers attach one receipt per call with
//! [`ToolOutput::with_verification`](crate::ToolOutput::with_verification).
//! Hosts read it back with [`VerificationReceipt::from_metadata`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The [`ToolOutput`](crate::ToolOutput) metadata key holding a receipt.
pub const VERIFICATION_METADATA_KEY: &str = "verification";

/// How much authority a reported exit status carries.
///
/// The default is [`Self::Absent`] because a tool that makes no explicit claim
/// has not established that its status means anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExitAuthority {
    /// The status reflects every stage of the command that ran.
    Authoritative,
    /// The status was inferred, for example from only the last stage of a pipeline.
    Derived,
    /// No exit status was available at all.
    #[default]
    Absent,
}

impl ExitAuthority {
    /// Whether this status may be cited as evidence on its own.
    #[must_use]
    pub const fn is_authoritative(self) -> bool {
        matches!(self, Self::Authoritative)
    }
}

/// What one recorded call proved.
///
/// The default is [`Self::Unknown`]: absent an explicit claim, a call proves nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReceiptOutcome {
    /// The call ran to completion and reported success.
    Passed,
    /// The call ran and reported failure.
    Failed,
    /// The call's result is not decidable from what the tool observed.
    #[default]
    Unknown,
}

/// One tool call's verification evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationReceipt {
    /// One line naming what ran, suitable for a checklist entry.
    pub summary: String,
    /// The directory the call ran in, when the tool controls one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// The process exit status, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// Whether `exit_code` covers the whole command.
    pub exit_authority: ExitAuthority,
    /// What the call proved.
    pub outcome: ReceiptOutcome,
    /// Repository revision observed while running, when the tool resolves one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    /// Digest of the captured output, so a citation can be checked for drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_digest: Option<String>,
    /// Why the outcome is what it is, when that is not obvious from the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl VerificationReceipt {
    /// A receipt for a call that ran and reported an authoritative success.
    #[must_use]
    pub fn passed(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            exit_code: Some(0),
            exit_authority: ExitAuthority::Authoritative,
            outcome: ReceiptOutcome::Passed,
            ..Self::default()
        }
    }

    /// A receipt for a call that ran and reported failure.
    #[must_use]
    pub fn failed(summary: impl Into<String>, exit_code: Option<i64>) -> Self {
        Self {
            summary: summary.into(),
            exit_code,
            exit_authority: ExitAuthority::Authoritative,
            outcome: ReceiptOutcome::Failed,
            ..Self::default()
        }
    }

    /// A receipt that records the call without claiming it proved anything.
    #[must_use]
    pub fn unknown(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            detail: Some(detail.into()),
            ..Self::default()
        }
    }

    /// Whether this receipt is usable as standalone evidence that work succeeded.
    #[must_use]
    pub const fn proves_success(&self) -> bool {
        matches!(self.outcome, ReceiptOutcome::Passed) && self.exit_authority.is_authoritative()
    }

    /// The JSON value stored under [`VERIFICATION_METADATA_KEY`].
    #[must_use]
    pub fn to_metadata_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Read a receipt back out of tool metadata.
    ///
    /// Returns `Ok(None)` when the tool attached no receipt.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] when a receipt is present but not decodable, so a
    /// malformed receipt is never silently treated as absent evidence.
    pub fn from_metadata(metadata: &Map<String, Value>) -> Result<Option<Self>, serde_json::Error> {
        match metadata.get(VERIFICATION_METADATA_KEY) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => serde_json::from_value(value.clone()).map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receipt_round_trips_through_tool_metadata() {
        let receipt = VerificationReceipt {
            summary: "cargo test --workspace".to_owned(),
            workdir: Some("/workspace".to_owned()),
            git_head: Some("abc123".to_owned()),
            ..VerificationReceipt::passed("ignored")
        };
        let mut metadata = Map::new();
        metadata.insert(
            VERIFICATION_METADATA_KEY.to_owned(),
            receipt.to_metadata_value(),
        );

        let decoded = VerificationReceipt::from_metadata(&metadata)
            .expect("decode receipt")
            .expect("receipt present");
        assert_eq!(decoded, receipt);
        assert!(decoded.proves_success());
    }

    #[test]
    fn absent_metadata_decodes_as_no_receipt() {
        assert_eq!(
            VerificationReceipt::from_metadata(&Map::new()).expect("decode empty metadata"),
            None
        );
    }

    #[test]
    fn a_malformed_receipt_is_an_error_rather_than_absent_evidence() {
        let mut metadata = Map::new();
        metadata.insert(
            VERIFICATION_METADATA_KEY.to_owned(),
            Value::String("passed".to_owned()),
        );

        assert!(VerificationReceipt::from_metadata(&metadata).is_err());
    }

    #[test]
    fn defaults_prove_nothing() {
        let receipt = VerificationReceipt::default();
        assert_eq!(receipt.outcome, ReceiptOutcome::Unknown);
        assert_eq!(receipt.exit_authority, ExitAuthority::Absent);
        assert!(!receipt.proves_success());
        assert!(!VerificationReceipt::failed("cargo test", Some(101)).proves_success());
        assert!(!VerificationReceipt::unknown("cargo test", "timed out").proves_success());
    }

    #[test]
    fn the_wire_shape_is_camel_case_without_absent_fields() {
        let value = VerificationReceipt::passed("cargo fmt").to_metadata_value();
        assert_eq!(value["exitAuthority"], "authoritative");
        assert_eq!(value["outcome"], "passed");
        assert_eq!(value["exitCode"], 0);
        assert!(value.get("workdir").is_none());
        assert!(value.get("gitHead").is_none());
    }
}

//! The instrument that proves or disproves this project's compatibility claim.
//!
//! `opencode-rust` promises to be a drop-in replacement for `opencode` v1.18.13:
//! same config, same CLI, same HTTP API, same on-disk state. Ninety-one later
//! tasks each verify some part of that promise through this crate. If this crate
//! can be satisfied by wrong code, the promise is unfounded — so every design
//! decision here is made in favour of *detecting* a difference rather than
//! producing a green run.
//!
//! # The failure this crate exists to prevent
//!
//! A reference Rust agent shipped an MCP stdio client that framed messages with
//! LSP-style `Content-Length: N\r\n\r\n<json>` headers. The MCP stdio transport is
//! newline-delimited JSON, so that client was non-functional against every real
//! MCP server — and its test suite was completely green. The fixtures it validated
//! against were Python scripts written to *emit and parse the same wrong framing*.
//! The tests proved that two components in one repository agreed with each other.
//! They could not, even in principle, say anything about the protocol.
//!
//! Three concrete rules follow, and they are enforced by types and tests rather
//! than by intention:
//!
//! 1. **Wire formats are validated against recorded real traffic.** The 40
//!    cassettes in the oracle tree were produced by real providers answering the
//!    real client. [`cassette`] reads them; [`MockProvider`] serves them; and when
//!    a response *had* to be written here instead,
//!    [`ResponseOrigin::Authored`] records that, with the reason.
//! 2. **The harness never makes a live provider call.** Not by discipline — this
//!    crate has no HTTP client in its dependency graph, so it has no capability to
//!    make one. `tests/no_http_client.rs` fails if one is added.
//!    [`ScriptedEnv`] additionally disables the oracle's own autoupdate and
//!    models-catalogue fetches at the process boundary.
//! 3. **Normalization is narrow, named, and pinned.** See [`normalize`]. A
//!    normalizer wide enough to force a pass is worse than no harness, so every
//!    rule is individually tested, the default set is pinned by a test, and
//!    [`DiffReport::render`] prints what was masked even when the diff passes.
//!
//! # The pieces
//!
//! | type | role |
//! |---|---|
//! | [`Oracle`] | the real `opencode`, as an installed binary or from the pinned source tree |
//! | [`Subject`] | this project's `opencode-rust` |
//! | [`ScriptedEnv`] | the closed world both sides run in: temp `XDG_*`, temp `HOME`, temp `TMPDIR`, explicit `OPENCODE_DB` |
//! | [`ConfigFixture`] | layered config trees on disk, for a config differential matrix |
//! | [`diff_normalized`] | the verdict, with provenance and masking in the report |
//! | [`CassettePlayer`] | cursor replay of the oracle's recorded provider traffic |
//! | [`MockProvider`] | a loopback provider stand-in that captures every request |
//!
//! # A differential in full
//!
//! ```no_run
//! use oc_testkit::{Oracle, Subject, diff_normalized};
//!
//! # fn main() -> Result<(), oc_testkit::TestkitError> {
//! let oracle = Oracle::discover()?;
//! let mut subject = Subject::discover_or_build()?;
//! subject.probe_version()?;
//!
//! // The version gap between the installed release and the pinned tree is a fact
//! // about the machine. Read it, print it, never assume it away.
//! println!("{}", oracle.version_gap().describe());
//!
//! let left = oracle.run(["debug", "paths"])?;
//! let right = subject.run(["debug", "paths"])?;
//! let report = diff_normalized(
//!     left.label(),
//!     &left.stdout,
//!     right.label(),
//!     &right.stdout,
//!     &oracle.env().normalizer(),
//! );
//! report.assert_identical();
//! # Ok(())
//! # }
//! ```

pub mod cassette;
pub mod config_fixture;
pub mod diff;
pub mod env;
pub mod error;
pub mod mock_provider;
pub mod normalize;
pub mod oracle;
pub mod run;
pub mod subject;

pub use crate::cassette::{
    BodyEncoding, Cassette, CassettePlayer, HttpInteraction, Interaction, RequestSnapshot,
    ResponseSnapshot, SseFrame, canonical_snapshot, list_cassettes, recordings_root,
};
pub use crate::config_fixture::{ConfigFixture, ConfigLayer, PlacedLayer};
pub use crate::diff::{DiffReport, Divergence, diff_normalized};
pub use crate::env::{DbChoice, ScriptedEnv};
pub use crate::error::{Result, TestkitError};
pub use crate::mock_provider::{
    CapturedRequest, MockProvider, MockResponse, ResponseOrigin, Scenario, StreamSignal,
};
pub use crate::normalize::{NormalizationRule, Normalizer};
pub use crate::oracle::{Oracle, OracleFlavour, requested_flavour};
pub use crate::run::{Provenance, RunOutcome, VersionGap};
pub use crate::subject::{SUBJECT_BIN, SUBJECT_PACKAGE, Subject};

/// Compare two [`RunOutcome`]s, labelling each side with its own provenance.
///
/// Prefer this over calling [`diff_normalized`] with hand-written labels: it is
/// what guarantees the oracle flavour and both version numbers appear in the
/// failure, so a patch-level version gap is never mistaken for a compatibility
/// defect.
#[must_use]
pub fn diff_runs(left: &RunOutcome, right: &RunOutcome, normalizer: &Normalizer) -> DiffReport {
    diff_normalized(
        left.label(),
        &left.stdout,
        right.label(),
        &right.stdout,
        normalizer,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn outcome(provenance: Provenance, stdout: &str) -> RunOutcome {
        RunOutcome {
            provenance,
            program: PathBuf::from("/x"),
            args: vec!["--version".to_owned()],
            working_dir: PathBuf::from("/"),
            exit_code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            env: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn diff_runs_carries_both_provenances_into_the_report() {
        let left = outcome(
            Provenance::OracleInstalledBinary {
                program: PathBuf::from("/usr/bin/opencode"),
                reported_version: "1.18.12".to_owned(),
                pinned_source_version: Some("1.18.13".to_owned()),
                pinned_source_commit: Some("aefaf140c1".to_owned()),
            },
            "1.18.12\n",
        );
        let right = outcome(
            Provenance::Subject {
                program: PathBuf::from("/t/opencode-rust"),
                reported_version: Some("0.1.0".to_owned()),
            },
            "0.1.0\n",
        );
        let report = diff_runs(&left, &right, &Normalizer::none());
        let rendered = report.render();
        assert!(!report.is_identical(), "the versions differ");
        assert!(rendered.contains("installed-binary"), "{rendered}");
        assert!(rendered.contains("pinned source 1.18.13"), "{rendered}");
        assert!(rendered.contains("opencode-rust"), "{rendered}");
    }
}

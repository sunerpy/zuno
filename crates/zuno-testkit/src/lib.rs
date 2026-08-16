//! The instrument behind this project's retained verification inventory.
//!
//! Zuno is an independent product. It does **not** promise to be a drop-in
//! replacement for `opencode`: its config root, data root, project directory, CLI
//! identity and on-disk state are its own, and it neither reads the old paths nor
//! imports opencode sessions. The one supported compatibility layer is the plugin
//! ABI — `engines.opencode`, the six `OPENCODE_*` handshake variables, and the
//! `COMPATIBILITY_VERSION` an npm plugin range-matches against.
//!
//! That decision has since been taken: the whole-surface differential suites that
//! byte-compared Zuno against the released `opencode` binary are **gone**. What
//! remains of this crate is the harness those suites happened to share — the
//! scripted environment, the cassette-backed mock provider, the diff engine, and
//! the pinned-release helpers — which 22 test files across 9 crates still use for
//! their own assertions, including the docs generator and the plugin-ABI gate.
//!
//! So the oracle in these APIs is now a *tool*, not a contract. A test may still
//! run the pinned release to source a fixture or to check the plugin handshake;
//! nothing here asserts that Zuno's own output must match it. Where a surface has
//! deliberately diverged, the difference is declared rather than normalised away —
//! every design decision in this crate is still made in favour of *detecting* a
//! difference rather than producing a green run.
//!
//! `1.18.13` is the **source baseline** — the tree this port was read from and the
//! version it reports to the npm plugin gate. The binary the differentials
//! actually execute is [`PINNED_RELEASE`], the newest installed release, currently
//! `1.18.18`. The two numbers are separate pins and [`oracle`] documents why;
//! recording one as though it were the other is the defect plan todo 130 closed.
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
//! | [`Subject`] | this project's `zuno` binary |
//! | [`ScriptedEnv`] | the closed world both sides run in: temp `XDG_*`, temp `HOME`, temp `TMPDIR`, explicit `OPENCODE_DB` |
//! | [`ConfigFixture`] | layered config trees on disk, for a config differential matrix |
//! | [`diff_normalized`] | the verdict, with provenance and masking in the report |
//! | [`normalize_cli_stream`] | the four declared CLI *presentation* differences, so `crates/zuno-cli/tests/cli_parity.rs` can compare every implemented command's streams |
//! | [`CassettePlayer`] | cursor replay of the oracle's recorded provider traffic |
//! | [`MockProvider`] | a loopback provider stand-in that captures every request |
//! | [`FakeTerminalOwner`] | a terminal-lease owner that records transitions and owns no TTY |
//!
//! # A differential in full
//!
//! ```no_run
//! use zuno_testkit::{Oracle, Subject, diff_normalized};
//!
//! # fn main() -> Result<(), zuno_testkit::TestkitError> {
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
pub mod cli_normalize;
pub mod compat_report;
pub mod config_fixture;
pub mod diff;
pub mod divergence;
pub mod env;
pub mod error;
pub mod mock_provider;
pub mod normalize;
pub mod oracle;
pub mod perf;
pub mod run;
pub mod subject;
pub mod terminal_owner;

pub use crate::cassette::{
    BodyEncoding, Cassette, CassettePlayer, HttpInteraction, Interaction, RequestSnapshot,
    ResponseSnapshot, SseFrame, canonical_snapshot, list_cassettes, recordings_root,
    recordings_root_or_skip,
};
pub use crate::cli_normalize::{
    CLI_RULE_NAMES, canonicalize_json, mask_program_name, normalize_cli_stream, strip_error_prefix,
    strip_prompt_chrome, strip_sgr,
};
pub use crate::compat_report::{
    BehaviouralDifference, ComparedSurface, CompatReport, DivergenceSummary, KnownGap,
    Normalization, OracleAvailability, OracleKind, Verdict,
};
pub use crate::config_fixture::{ConfigFixture, ConfigLayer, PlacedLayer};
pub use crate::diff::{DiffReport, Divergence, diff_normalized};
pub use crate::divergence::{
    DECLARED_COUNT, DeclaredDivergence, DivergenceList, EXECUTE_CONTRACT_ID, ExecuteContract,
};
pub use crate::env::{DbChoice, ScriptedEnv};
pub use crate::error::{Result, TestkitError};
pub use crate::mock_provider::{
    CapturedRequest, MockProvider, MockResponse, ResponseOrigin, Scenario, StreamSignal,
};
pub use crate::normalize::{NormalizationRule, Normalizer};
pub use crate::oracle::{
    Oracle, OracleFlavour, PINNED_RELEASE, PinnedOracle, check_pin, pinned_oracle,
    pinned_oracle_or_skip, requested_flavour,
};
pub use crate::run::{Provenance, RunOutcome, VersionGap};
pub use crate::subject::{SUBJECT_BIN, SUBJECT_PACKAGE, Subject};
pub use crate::terminal_owner::{FakeTerminalOwner, TerminalTranscript, TerminalTransition};

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

    /// The two provenance version numbers must both survive into the report, which is
    /// what stops a patch-level gap being read as a compatibility defect.
    ///
    /// The pair used here is this machine's real one — the installed release
    /// [`PINNED_RELEASE`] against the `1.18.13` source tree at `aefaf140c1` — rather
    /// than an invented pair, so the fixture cannot go on depicting a gap the machine
    /// no longer has.
    #[test]
    fn diff_runs_carries_both_provenances_into_the_report() {
        let left = outcome(
            Provenance::OracleInstalledBinary {
                program: PathBuf::from("/usr/bin/opencode"),
                reported_version: PINNED_RELEASE.to_owned(),
                pinned_source_version: Some("1.18.13".to_owned()),
                pinned_source_commit: Some("aefaf140c1".to_owned()),
            },
            format!("{PINNED_RELEASE}\n").as_str(),
        );
        let right = outcome(
            Provenance::Subject {
                program: PathBuf::from("/t/zuno"),
                source: crate::run::SubjectSource::ExplicitPath,
                reported_version: Some("0.1.0".to_owned()),
            },
            "0.1.0\n",
        );
        let report = diff_runs(&left, &right, &Normalizer::none());
        let rendered = report.render();
        assert!(!report.is_identical(), "the versions differ");
        assert!(rendered.contains("installed-binary"), "{rendered}");
        assert!(rendered.contains("pinned source 1.18.13"), "{rendered}");
        assert!(rendered.contains("zuno"), "{rendered}");
    }
}

#[cfg(test)]
mod cassettes;

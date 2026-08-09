//! The compatibility suite's machine-readable report.
//!
//! # Why the report is part of the gate rather than a log
//!
//! A green suite answers "did anything I checked disagree?". It does not answer
//! "what did you check?" — and for a drop-in-replacement claim that second
//! question is the load-bearing one. A suite that silently stopped comparing a
//! surface, or that never compared it at all, is green in exactly the same way as
//! one that compared everything.
//!
//! So the suite emits this artifact and each surface carries a [`Verdict`]. A
//! reader (or plan todos F1-F4) can then see the difference between *compared and
//! equal*, *compared with a declared exception*, and *never compared*, instead of
//! inferring all three from the absence of a failure.
//!
//! # Why normalizations are enumerated
//!
//! Every differential comparison scrubs volatile values — ids, timestamps,
//! absolute paths, ports. Each scrub is a small licence to differ, and a large
//! enough pile of them makes any two programs agree. Listing them with a reason
//! puts the pile in the artifact, where it can be argued with. Plan todo 86's
//! "must not normalize away a real difference" is only checkable if the
//! normalizations are visible.
//!
//! # Why gaps are here and not in `docs/divergences.toml`
//!
//! An unimplemented surface is not a decision. Recording it as a declared
//! divergence would dress an omission up as a design choice; recording it as a
//! [`KnownGap`] keeps it legible as work outstanding. See
//! [`crate::divergence`].

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Result, TestkitError};

/// Where the report is written when `OC_COMPAT_REPORT` is unset.
pub const DEFAULT_RELATIVE_PATH: &str = "target/compat/compat-report.json";

/// Environment variable overriding the report's destination.
pub const PATH_ENV: &str = "OC_COMPAT_REPORT";

/// The artifact's format version, so a consumer can refuse a shape it predates.
pub const SCHEMA_VERSION: u32 = 1;

/// How thoroughly one surface was compared against the oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Compared against the oracle and equal after the listed normalizations.
    Compared,
    /// Compared, and the difference found is a declared divergence or a recorded
    /// gap. The entry's `detail` names which.
    PartiallyCompared,
    /// Not compared. `detail` must say why, because an unexplained omission here
    /// is indistinguishable from an oversight.
    NotCompared,
    /// A comparison exists but could not run in this environment — typically the
    /// oracle binary or a language server is absent. Never silent: the suite
    /// prints the skip as well as recording it.
    Skipped,
}

/// What the comparison was made against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleKind {
    /// The real `opencode` binary, executed.
    LiveBinary,
    /// A committed capture of the real binary's output.
    CommittedFixture,
    /// The upstream TypeScript source tree, read.
    SourceTree,
    /// A real third-party counterpart, executed (a language server, an MCP server).
    LiveCounterpart,
    /// No oracle. Only valid with [`Verdict::NotCompared`].
    None,
}

/// One compared surface.
#[derive(Debug, Clone, Serialize)]
pub struct ComparedSurface {
    /// Stable identifier, so a consumer can diff two reports.
    pub id: String,
    /// Human-readable name of the surface.
    pub name: String,
    /// How thoroughly it was compared.
    pub verdict: Verdict,
    /// What it was compared against.
    pub oracle: OracleKind,
    /// The test that performs the comparison, as `path::test_name`, so a reader
    /// can run it directly rather than trusting this line.
    pub evidence: String,
    /// What was actually established, or why nothing was.
    pub detail: String,
    /// Counts the comparison measured. Present so a later report can be diffed
    /// against this one numerically.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measured: Vec<(String, u64)>,
}

/// A volatile value a comparison scrubs before diffing.
#[derive(Debug, Clone, Serialize)]
pub struct Normalization {
    /// Which surface applies it.
    pub surface: String,
    /// What is scrubbed.
    pub value: String,
    /// Why scrubbing it does not hide a real difference.
    pub reason: String,
}

/// A behavioural difference from upstream, bound to the allow-list entry for it.
///
/// # Why this type replaced `NominatedDivergence`
///
/// Until plan todo 119 this was a *nomination*: a difference recorded here
/// precisely because it was NOT in `docs/divergences.toml`, with the suite
/// asserting it stayed out so the plan's declared count kept holding. That made a
/// second reporting structure for the same kind of fact, and — because the
/// assertion was that the id is *absent* from the allow-list — no gate could ever
/// fail when a real behavioural difference went undeclared. A reader consulting the
/// declared allow-list learned nothing about six of them.
///
/// Now every record names, in [`BehaviouralDifference::declared_as`], the entry in
/// the allow-list that must cover it. The gate resolves that id against the loaded
/// file and fails when it is missing, so the allow-list is the single place and this
/// list is only the index into it. Where two differences turned out to be one
/// decision, they share a `declared_as` rather than being declared twice.
#[derive(Debug, Clone, Serialize)]
pub struct BehaviouralDifference {
    /// Stable id of the difference, kept from the nomination it replaces.
    pub id: String,
    /// The surface affected.
    pub surface: String,
    /// The id in `docs/divergences.toml` that must declare this difference.
    ///
    /// Resolved against the loaded allow-list by the gate. A difference whose entry
    /// is absent, renamed or deleted fails there.
    pub declared_as: String,
    /// The upstream file and lines that make this a difference rather than a guess.
    pub upstream_evidence: String,
    /// The test that proves the divergent behaviour is live, not merely written down.
    pub asserted_by: String,
}

/// A surface where this implementation is behind upstream, with no decision behind it.
#[derive(Debug, Clone, Serialize)]
pub struct KnownGap {
    /// Stable identifier.
    pub id: String,
    /// The surface affected.
    pub surface: String,
    /// Exactly what is missing, and where it is (or is not) served instead.
    pub detail: String,
}

/// The oracle's availability in the environment that produced this report.
#[derive(Debug, Clone, Serialize)]
pub struct OracleAvailability {
    /// Whether the real binary was found.
    pub available: bool,
    /// Where it was found, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The version it reported, when it was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The version this port is pinned to compare against.
    pub pinned_source_version: String,
}

/// The allow-list's state, as the suite observed it.
#[derive(Debug, Clone, Serialize)]
pub struct DivergenceSummary {
    /// Entries the file declares.
    pub declared_count: usize,
    /// Entries [`crate::divergence::DECLARED_COUNT`] expects.
    pub expected_count: usize,
    /// Every declared id, sorted.
    pub ids: Vec<String>,
}

/// The whole artifact.
#[derive(Debug, Clone, Serialize)]
pub struct CompatReport {
    /// Format version.
    pub schema_version: u32,
    /// The command that produces this artifact, restated so a reader can rerun it.
    pub generated_by: &'static str,
    /// Oracle availability at generation time.
    pub oracle: OracleAvailability,
    /// The allow-list's observed state.
    pub divergences: DivergenceSummary,
    /// Every surface, in declaration order.
    pub surfaces: Vec<ComparedSurface>,
    /// Every normalization applied anywhere in the suite.
    pub normalizations: Vec<Normalization>,
    /// Every behavioural difference, each naming the allow-list entry declaring it.
    pub behavioural_differences: Vec<BehaviouralDifference>,
    /// Every surface where this port is behind upstream.
    pub known_gaps: Vec<KnownGap>,
}

impl CompatReport {
    /// Surfaces carrying the given verdict.
    #[must_use]
    pub fn with_verdict(&self, verdict: Verdict) -> Vec<&ComparedSurface> {
        self.surfaces
            .iter()
            .filter(|surface| surface.verdict == verdict)
            .collect()
    }

    /// The report's destination: `OC_COMPAT_REPORT`, else under the workspace root.
    ///
    /// # Errors
    ///
    /// Fails when neither is resolvable, which means the harness cannot say where
    /// it would have written its evidence — worth failing over rather than
    /// discarding the artifact.
    pub fn destination() -> Result<PathBuf> {
        if let Ok(explicit) = std::env::var(PATH_ENV)
            && !explicit.trim().is_empty()
        {
            return Ok(PathBuf::from(explicit));
        }
        let root = crate::subject::workspace_root().ok_or_else(|| TestkitError::Io {
            action: "locate the workspace root for the compatibility report".to_owned(),
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no ancestor Cargo.toml declares [workspace]",
            ),
        })?;
        Ok(root.join(DEFAULT_RELATIVE_PATH))
    }

    /// Writes the report as pretty-printed JSON, creating parent directories.
    ///
    /// Pretty rather than compact because the primary consumer is a human asking
    /// "what was proven?", and a diff between two runs should be readable.
    ///
    /// # Errors
    ///
    /// Fails when the parent directory cannot be created or the file cannot be
    /// written.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| TestkitError::ReportWrite {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let mut json =
            serde_json::to_string_pretty(self).map_err(|source| TestkitError::ReportWrite {
                path: path.to_path_buf(),
                source: std::io::Error::other(source),
            })?;
        json.push('\n');
        std::fs::write(path, json).map_err(|source| TestkitError::ReportWrite {
            path: path.to_path_buf(),
            source,
        })
    }

    /// A one-screen summary for a test's stdout.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        out.push_str("compatibility report\n");
        for surface in &self.surfaces {
            out.push_str(&format!(
                "  [{:>18}] {} — {}\n",
                serde_json::to_value(surface.verdict)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                surface.id,
                surface.detail
            ));
        }
        out.push_str(&format!(
            "  {} normalization(s), {} known gap(s), {} declared divergence(s), \
             {} behavioural difference(s) each resolved to a declared entry\n",
            self.normalizations.len(),
            self.known_gaps.len(),
            self.divergences.declared_count,
            self.behavioural_differences.len()
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(id: &str, verdict: Verdict) -> ComparedSurface {
        ComparedSurface {
            id: id.to_owned(),
            name: id.to_owned(),
            verdict,
            oracle: OracleKind::LiveBinary,
            evidence: "tests/x.rs::y".to_owned(),
            detail: "d".to_owned(),
            measured: Vec::new(),
        }
    }

    fn report() -> CompatReport {
        CompatReport {
            schema_version: SCHEMA_VERSION,
            generated_by: "cargo test --test compat_suite",
            oracle: OracleAvailability {
                available: false,
                path: None,
                version: None,
                pinned_source_version: "1.18.13".to_owned(),
            },
            divergences: DivergenceSummary {
                declared_count: 7,
                expected_count: 7,
                ids: vec!["a".to_owned()],
            },
            surfaces: vec![
                surface("one", Verdict::Compared),
                surface("two", Verdict::NotCompared),
            ],
            normalizations: Vec::new(),
            behavioural_differences: Vec::new(),
            known_gaps: Vec::new(),
        }
    }

    #[test]
    fn the_report_round_trips_to_a_file_and_names_every_surface() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("report.json");
        report().write(&path).expect("write");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("\"schema_version\": 1"), "{text}");
        assert!(text.contains("\"id\": \"one\""), "{text}");
        assert!(text.contains("\"verdict\": \"not_compared\""), "{text}");
        assert!(
            text.ends_with('\n'),
            "the artifact must be newline-terminated"
        );
    }

    #[test]
    fn verdict_filtering_separates_compared_from_uncompared() {
        let report = report();
        assert_eq!(report.with_verdict(Verdict::Compared).len(), 1);
        assert_eq!(report.with_verdict(Verdict::NotCompared).len(), 1);
        assert_eq!(report.with_verdict(Verdict::Skipped).len(), 0);
    }

    #[test]
    fn the_default_destination_resolves_under_the_workspace_root() {
        let resolved = CompatReport::destination().expect("a destination must resolve");
        assert!(
            resolved.ends_with("compat-report.json"),
            "unexpected default: {}",
            resolved.display()
        );
    }
}

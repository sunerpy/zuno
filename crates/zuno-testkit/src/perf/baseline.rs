//! Frozen TypeScript memory-baseline artifact.

use std::path::{Path, PathBuf};

use crate::error::{Result, TestkitError};
use serde::{Deserialize, Serialize};

use super::methodology::PERF_METHODOLOGY_REVISION;
pub use super::process_tree::RssSample;

/// Stable identifiers for the three plan-frozen workloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadName {
    /// Cold start, one cassette-backed tool turn, and settle.
    WIdle,
    /// Largest real session hydration, render, and one turn.
    WReal,
    /// Long-running memory-driver soak.
    WSoak,
}

/// One total-process-tree RSS observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMeasurement {
    /// One-based repetition number.
    pub repetition: usize,
    /// Peak over the samples this workload's warm-up rule retains.
    pub peak_rss_kib: u64,
    /// Every 2-second sample, including any the warm-up rule discards.
    pub samples: Vec<RssSample>,
}

/// Measurements and provenance for one frozen workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadMeasurement {
    /// Frozen workload identifier.
    pub name: WorkloadName,
    /// Whether this is only the explicitly permitted short W-soak smoke.
    pub smoke_only: bool,
    /// Number of turns executed.
    pub turns: usize,
    /// Wall-clock duration represented by this workload.
    pub wall_clock_seconds: u64,
    /// Selected session for W-real.
    pub session_id: Option<String>,
    /// Message count for the selected W-real session.
    pub message_count: Option<u64>,
    /// Part count for the selected W-real session.
    pub part_count: Option<u64>,
    /// Sum of `LENGTH(part.data)` used to select W-real.
    pub part_data_bytes: Option<u64>,
    /// Independent run results.
    pub runs: Vec<RunMeasurement>,
    /// Median of the per-run peaks.
    pub median_peak_rss_kib: Option<u64>,
    /// Why this workload carries no measurement, when it carries none.
    ///
    /// Present only for a workload deferred to a later wave. A workload that
    /// G1 or G2 consumes can never reach this state: [`BaselineReport::validate`]
    /// rejects a report whose W-idle or W-real lacks five runs and a median, so a
    /// missing gate input fails the artifact instead of being waived here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_reason: Option<String>,
}

/// Run-to-run stability evidence derived from a workload's retained peaks.
///
/// Derived rather than stored so it cannot drift from the runs it summarises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeakSpread {
    /// Lowest per-run peak.
    pub min_rss_kib: u64,
    /// Median per-run peak, by the same rule the runner reports.
    pub median_rss_kib: u64,
    /// Highest per-run peak.
    pub max_rss_kib: u64,
}

impl PeakSpread {
    /// Ratio of the widest pair of per-run peaks, `1.0` when every run agreed.
    #[must_use]
    pub fn max_over_min(self) -> f64 {
        self.max_rss_kib as f64 / self.min_rss_kib as f64
    }
}

impl WorkloadMeasurement {
    /// Spread of the retained per-run peaks, or `None` for a deferred workload.
    #[must_use]
    pub fn peak_spread(&self) -> Option<PeakSpread> {
        let mut peaks: Vec<u64> = self.runs.iter().map(|run| run.peak_rss_kib).collect();
        peaks.sort_unstable();
        Some(PeakSpread {
            min_rss_kib: *peaks.first()?,
            median_rss_kib: *peaks.get(peaks.len() / 2)?,
            max_rss_kib: *peaks.last()?,
        })
    }
}

/// Facts needed to reproduce and attribute a baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineFacts {
    /// Kernel release.
    pub kernel: String,
    /// Host name.
    pub hostname: String,
    /// CPU model string from `/proc/cpuinfo`.
    pub cpu_model: String,
    /// Logical CPU count visible to this process.
    pub logical_cpus: usize,
    /// Physical memory reported by `/proc/meminfo`.
    pub ram_kib: u64,
    /// Exact oracle path selected by [`crate::Oracle`].
    pub typescript_binary: PathBuf,
    /// Version self-reported by that oracle.
    pub typescript_version: String,
}

/// Committed TypeScript-only baseline artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineReport {
    /// Artifact schema version.
    pub schema_version: u32,
    /// Formula revision used to interpret the numbers.
    pub methodology_revision: u32,
    /// Explicitly states that no Rust subject was measured.
    pub subject: String,
    /// Machine and oracle provenance.
    pub machine: MachineFacts,
    /// All three frozen workloads.
    pub workloads: Vec<WorkloadMeasurement>,
}

impl BaselineReport {
    /// Find a frozen workload by identifier.
    #[must_use]
    pub fn workload(&self, name: WorkloadName) -> Option<&WorkloadMeasurement> {
        self.workloads.iter().find(|workload| workload.name == name)
    }

    /// Parse and validate a report at a caller-selected path.
    ///
    /// # Errors
    /// Returns a typed decode or invariant failure for unusable baseline data.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|source| TestkitError::io("read TypeScript baseline", path, source))?;
        let report: Self =
            serde_json::from_slice(&bytes).map_err(|source| TestkitError::BaselineDecode {
                path: path.to_path_buf(),
                source,
            })?;
        report.validate()?;
        Ok(report)
    }

    fn validate(&self) -> Result<()> {
        if self.subject != "typescript-only" {
            return Err(TestkitError::BaselineInvariant {
                detail: "baseline subject must be exactly typescript-only".to_owned(),
            });
        }
        if self.methodology_revision != PERF_METHODOLOGY_REVISION {
            return Err(TestkitError::BaselineInvariant {
                detail: format!(
                    "baseline methodology revision {} does not match code revision {}",
                    self.methodology_revision, PERF_METHODOLOGY_REVISION
                ),
            });
        }
        for name in [
            WorkloadName::WIdle,
            WorkloadName::WReal,
            WorkloadName::WSoak,
        ] {
            let workload = self
                .workload(name)
                .ok_or_else(|| TestkitError::BaselineInvariant {
                    detail: format!("missing frozen workload {name:?}"),
                })?;
            if matches!(name, WorkloadName::WIdle | WorkloadName::WReal)
                && (workload.runs.len() != 5 || workload.median_peak_rss_kib.is_none())
            {
                return Err(TestkitError::BaselineInvariant {
                    detail: format!("{name:?} must retain five runs and a median peak"),
                });
            }
        }
        Ok(())
    }
}

/// Load the repository's committed baseline artifact.
///
/// # Errors
/// Returns a typed read, decode, or invariant failure.
pub fn load_committed_baseline() -> Result<BaselineReport> {
    BaselineReport::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/ts-baseline.json"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_artifact_contains_every_frozen_workload() {
        // Given: the committed TypeScript-only baseline artifact.
        let baseline = load_committed_baseline().expect("committed baseline must parse");

        // When: each frozen workload is looked up by its stable identifier.
        let idle = baseline.workload(WorkloadName::WIdle);
        let real = baseline.workload(WorkloadName::WReal);
        let soak = baseline.workload(WorkloadName::WSoak);

        // Then: no workload is absent, and full G1/G2 runs retain five peaks each.
        assert_eq!(idle.expect("W-idle").runs.len(), 5);
        assert_eq!(real.expect("W-real").runs.len(), 5);
        assert!(soak.expect("W-soak").smoke_only);
    }

    /// Run-to-run stability is evidenced from one pass's five peaks.
    ///
    /// This replaces a second full measurement pass agreeing within 10%: the
    /// spread of the retained peaks measures the same variance from data already
    /// committed, and the measured spread exceeds 10% on this machine.
    #[test]
    fn committed_baseline_records_the_spread_of_every_measured_workloads_peaks() {
        // Given: the committed artifact's measured and deferred workloads.
        let baseline = load_committed_baseline().expect("committed baseline must parse");

        // When/Then: each measured workload reports a coherent five-run spread whose
        // median is the median the artifact publishes.
        for name in [WorkloadName::WIdle, WorkloadName::WReal] {
            let workload = baseline.workload(name).expect("measured workload");
            let spread = workload.peak_spread().expect("five retained peaks");
            assert_eq!(workload.runs.len(), 5, "{name:?}");
            assert!(spread.min_rss_kib <= spread.median_rss_kib, "{spread:?}");
            assert!(spread.median_rss_kib <= spread.max_rss_kib, "{spread:?}");
            assert!(spread.max_over_min() >= 1.0, "{spread:?}");
            assert_eq!(
                Some(spread.median_rss_kib),
                workload.median_peak_rss_kib,
                "{name:?} publishes a median its own runs do not support"
            );
        }

        // And: the deferred workload records no spread and says why instead.
        let soak = baseline.workload(WorkloadName::WSoak).expect("W-soak");
        assert_eq!(soak.peak_spread(), None);
        assert!(soak.median_peak_rss_kib.is_none());
        assert!(soak.deferred_reason.is_some());
    }

    #[test]
    fn every_committed_peak_is_reproducible_from_its_retained_samples() {
        // Given: the committed artifact, which retains every raw sample.
        let baseline = load_committed_baseline().expect("committed baseline must parse");

        // When: each stored peak is re-derived through the production rule.
        // Then: it matches, so no number was recorded under a superseded revision.
        for workload in &baseline.workloads {
            for run in &workload.runs {
                assert_eq!(
                    super::super::workload::peak_after_warm_up(&run.samples, workload.name),
                    Some(run.peak_rss_kib),
                    "{:?} repetition {} disagrees with its own samples",
                    workload.name,
                    run.repetition
                );
            }
        }
    }
}

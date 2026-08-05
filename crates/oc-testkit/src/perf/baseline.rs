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
    /// Peak after the frozen 90-second warm-up.
    pub peak_rss_kib: u64,
    /// Every 2-second sample, including discarded warm-up samples.
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
    /// Median of per-run peaks after warm-up.
    pub median_peak_rss_kib: Option<u64>,
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
}

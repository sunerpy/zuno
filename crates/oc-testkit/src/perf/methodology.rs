//! Frozen threshold substitution and methodology hash lock.

use crate::error::{Result, TestkitError};

use super::baseline::{BaselineReport, WorkloadName};

/// Sampling-methodology revision assigned to the committed formula digest.
pub const PERF_METHODOLOGY_REVISION: u32 = 1;
#[cfg(test)]
const FORMULA_START: &str = "<!-- PERF_FORMULAS_START -->";
#[cfg(test)]
const FORMULA_END: &str = "<!-- PERF_FORMULAS_END -->";
#[cfg(test)]
const REVISION_1_HASH: &str = "db49ffeb3a19a265a948e5545afe14e245f8ac7c8201ae1b1e1748e87f6922ad";

/// Numeric right-hand sides of the four frozen gate predicates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrozenThresholds {
    /// G1 maximum Rust median peak, MiB.
    pub g1_max_mib: f64,
    /// G2 maximum Rust median peak, MiB.
    pub g2_max_mib: f64,
    /// G3 maximum Theil-Sen growth, MiB per turn.
    pub g3_max_mib_per_turn: f64,
    /// G3 maximum final-to-middle peak ratio.
    pub g3_max_peak_ratio: f64,
    /// G4 maximum seconds without state progress.
    pub g4_progress_timeout_seconds: f64,
    /// G4 absolute per-turn deadline in seconds.
    pub g4_hard_deadline_seconds: f64,
}

impl FrozenThresholds {
    /// Substitute measured TS medians into the frozen formulas.
    ///
    /// # Errors
    /// Returns an invariant failure when either required median is absent.
    pub fn from_baseline(baseline: &BaselineReport) -> Result<Self> {
        let median_mib = |name| {
            baseline
                .workload(name)
                .and_then(|workload| workload.median_peak_rss_kib)
                .map(|kib| kib as f64 / 1024.0)
                .ok_or_else(|| TestkitError::BaselineInvariant {
                    detail: format!("{name:?} has no measured median peak"),
                })
        };
        Ok(Self {
            g1_max_mib: 0.50 * median_mib(WorkloadName::WIdle)?,
            g2_max_mib: 0.50 * median_mib(WorkloadName::WReal)?,
            g3_max_mib_per_turn: 1.0,
            g3_max_peak_ratio: 1.5,
            g4_progress_timeout_seconds: 120.0,
            g4_hard_deadline_seconds: 1800.0,
        })
    }

    /// True only when every comparison threshold is finite and positive.
    #[must_use]
    pub fn all_finite(self) -> bool {
        [
            self.g1_max_mib,
            self.g2_max_mib,
            self.g3_max_mib_per_turn,
            self.g3_max_peak_ratio,
            self.g4_progress_timeout_seconds,
            self.g4_hard_deadline_seconds,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
    }
}

#[cfg(test)]
fn methodology_formula_section() -> Result<&'static str> {
    let doc = include_str!("../../../../docs/perf-methodology.md");
    let (_, rest) = doc
        .split_once(FORMULA_START)
        .ok_or(TestkitError::MethodologyFormulaSection)?;
    let (section, _) = rest
        .split_once(FORMULA_END)
        .ok_or(TestkitError::MethodologyFormulaSection)?;
    Ok(section.trim())
}

#[cfg(test)]
fn methodology_hash(bytes: &[u8]) -> String {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("sha256sum must be installed for the methodology lock test");
    child
        .stdin
        .take()
        .expect("sha256sum stdin must be piped")
        .write_all(bytes)
        .expect("formula section must be writable to sha256sum");
    let output = child
        .wait_with_output()
        .expect("sha256sum must complete successfully");
    assert!(output.status.success(), "sha256sum failed: {output:?}");
    String::from_utf8(output.stdout)
        .expect("sha256sum output must be UTF-8")
        .split_whitespace()
        .next()
        .expect("sha256sum output must contain a digest")
        .to_owned()
}

#[cfg(test)]
const fn expected_methodology_hash(revision: u32) -> &'static str {
    match revision {
        1 => REVISION_1_HASH,
        _ => "UNREGISTERED_METHODOLOGY_REVISION",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::baseline::load_committed_baseline;

    #[test]
    fn frozen_formulas_produce_finite_numeric_thresholds() {
        // Given: measured TypeScript medians from the committed baseline.
        let baseline = load_committed_baseline().expect("committed baseline must parse");

        // When: Wave 14's only permitted operation is applied: substitution.
        let thresholds = FrozenThresholds::from_baseline(&baseline).expect("finite thresholds");

        // Then: all four gates reduce to finite numeric comparisons.
        assert!(thresholds.all_finite(), "{thresholds:?}");
    }

    #[test]
    fn methodology_formula_section_matches_its_revision_hash() {
        // Given: the formula section committed with this methodology revision.
        let section = methodology_formula_section().expect("formula section must be delimited");

        // When: its SHA-256 digest is computed.
        let actual = methodology_hash(section.as_bytes());

        // Then: it matches the immutable digest assigned to this revision.
        assert_eq!(actual, expected_methodology_hash(PERF_METHODOLOGY_REVISION));
    }
}

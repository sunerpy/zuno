//! Reproducible non-functional measurements against the TypeScript oracle.

pub mod baseline;
mod database;
mod fixtures;
mod methodology;
mod process_tree;
mod runner;
mod workload;
#[cfg(test)]
mod workload_tests;

pub use baseline::{
    BaselineReport, MachineFacts, PeakSpread, RssSample, RunMeasurement, WorkloadMeasurement,
    WorkloadName, load_committed_baseline,
};
pub use methodology::{FrozenThresholds, PERF_METHODOLOGY_REVISION};
pub use runner::{
    BaselineRunOptions, PairedSide, interleaved_pair_order, measure_typescript_baseline,
    verify_typescript_oracle,
};

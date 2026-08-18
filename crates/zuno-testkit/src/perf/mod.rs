//! Reproducible non-functional measurements against the TypeScript oracle.

pub mod baseline;
mod database;
mod fixtures;
mod methodology;
mod process_tree;
mod runner;
pub mod subject;
mod workload;
#[cfg(test)]
mod workload_tests;

pub use baseline::{
    BaselineReport, MachineFacts, PeakSpread, RssSample, RunMeasurement, WorkloadMeasurement,
    WorkloadName, load_committed_baseline,
};
pub use database::verify_pinned_database;
pub use fixtures::create_watcher_tree;
pub use methodology::{FrozenThresholds, PERF_METHODOLOGY_REVISION};
pub use process_tree::sample_process_tree;
pub use runner::{
    BaselineRunOptions, PairedSide, interleaved_pair_order, measure_g1_g2_subject,
    measure_typescript_baseline, verify_typescript_oracle,
};
pub use subject::{PinnedSubject, W_REAL_RECAPTURE, W_REAL_SUBJECT};

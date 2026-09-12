//! Generate the client contract from Rust, independently of internal Worker DTOs.
use schemars::{JsonSchema, schema_for};
use zuno_types::activity::{CommittedFrame, FramePage, HistoryPage, LiveFrame};

#[derive(JsonSchema)]
#[allow(dead_code)]
struct ActivityProtocol {
    history: HistoryPage,
    frames: FramePage,
    committed: CommittedFrame,
    live: LiveFrame,
}

fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&schema_for!(ActivityProtocol)).expect("schema serializes")
    );
}

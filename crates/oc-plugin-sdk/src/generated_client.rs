//! Model/Provider-bearing arrivals derived from the pinned generated-client contract.
//!
//! The enum is generated from the success-response schema graph in the committed
//! OpenAPI capture. Todo 178 proved that capture byte-identical to release 1.18.18,
//! which is the same document from which the JavaScript SDK declarations are
//! generated. Adding a relevant generated response therefore adds an enum variant
//! during `cargo build`; every exhaustive consumer must classify it before the
//! workspace can compile.

include!(concat!(env!("OUT_DIR"), "/generated_client_arrivals.rs"));

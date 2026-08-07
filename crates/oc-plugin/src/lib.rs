//! Plugin host: the hook bus and the plugin lifecycle.

mod auth;
mod config_tools;
mod discovery;
mod hooks;
mod js;
mod jsonrpc;
mod manifest;
mod payload;
mod provider;

pub use crate::auth::*;
pub use crate::config_tools::*;
pub use crate::discovery::*;
pub use crate::hooks::*;
pub use crate::js::*;
pub use crate::jsonrpc::*;
pub use crate::manifest::*;
pub use crate::payload::*;
pub use crate::provider::*;

#[cfg(feature = "wasm")]
pub mod wasm;
#[cfg(feature = "wasm")]
pub use crate::wasm::*;

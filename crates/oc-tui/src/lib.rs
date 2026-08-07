//! Terminal user interface: views, keybindings, themes, and the render loop.
//!
//! [`attention`] ships no audio. Its built-in sound pack is registered and empty,
//! because upstream's six cues are `.mp3` files imported from the excluded
//! `@opencode-ai/ui` package and no licence can be stated for them; that module's
//! header documents how to supply a pack.
//!
//! [`views`] reproduces upstream's view **capabilities** rather than its layout —
//! the 204-file reference is a capability inventory, not a porting target. Every
//! view there paints only from [`theme`]'s resolved palette and acts only on
//! [`keybind`]'s resolved actions, and no dialog awaits its answer inside the event
//! loop; that module's header states why each of those is a correctness property
//! and not a style preference.

pub mod app;
pub mod attention;
pub mod config;
pub mod keybind;
pub mod theme;
pub mod views;

//! Terminal user interface: views, keybindings, themes, and the render loop.
//!
//! [`attention`] ships no audio. Its built-in sound pack is registered and empty,
//! because upstream's six cues are `.mp3` files imported from the excluded
//! `@opencode-ai/ui` package and no licence can be stated for them; that module's
//! header documents how to supply a pack.

pub mod app;
pub mod attention;
pub mod config;
pub mod keybind;
pub mod theme;

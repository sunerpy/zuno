//! The one skill that ships inside the binary.
//!
//! The model's intuition for `zuno.json` is often wrong and Zuno hard-fails on
//! invalid config, so this skill documents the native forms. It is registered
//! before disk discovery. A disk skill may use the same name, but remains a
//! separate source that must be selected explicitly.
//!
//! Two things here have to be exact rather than approximately right:
//!
//! * `location` is the literal string `<built-in>` — not a path. It is the only
//!   `location` in the set that is not a filesystem path, which is why the
//!   verbose render form HTML-escapes locations at all.
//! * `description` names only configuration surfaces implemented by Zuno.

use crate::skill::Skill;

/// The built-in skill's name.
pub const NAME: &str = "customize-zuno";

/// The catalog sentinel for a skill compiled into the binary.
pub const LOCATION: &str = "<built-in>";

/// Model-facing trigger for the native configuration skill.
pub const DESCRIPTION: &str = "Use ONLY when the user is editing or creating Zuno's own configuration or extensions: zuno.json, zuno.jsonc, files under .zuno/, files under ~/.config/zuno/, or process-local extension packages. Also use when creating or fixing Zuno agents, subagents, workflows, commands, skills, MCP servers, web search, or permission rules. Do not use for the user's own application code, or for any project that is not configuring Zuno itself.";

/// The native configuration guide embedded in the binary.
pub const CONTENT: &str = include_str!("customize-zuno.md");

/// The built-in skill, as it appears in `Skill.all()`.
#[must_use]
pub fn skill() -> Skill {
    Skill::embedded(NAME, Some(DESCRIPTION.to_string()), LOCATION, CONTENT)
}

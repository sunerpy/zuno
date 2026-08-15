//! The one skill that ships inside the binary.
//!
//! `skill/index.ts:27-35` and `:276-283`. The model's intuition for
//! `opencode.json` is often wrong and opencode hard-fails on invalid config, so
//! this skill hands it the real schemas. It is registered **before** disk
//! discovery so a user's own `customize-opencode` on disk overrides it.
//!
//! Two things here have to be exact rather than approximately right:
//!
//! * `location` is the literal string `<built-in>` — not a path. It is the only
//!   `location` in the set that is not a filesystem path, which is why the
//!   verbose render form HTML-escapes locations at all.
//! * `description` is the copy at `skill/index.ts:33-34`, which is **not** the
//!   same string as the v2 plugin's at `packages/core/src/plugin/skill.ts:23`:
//!   the v2 copy also lists `commands`. `opencode debug skill` 1.18.13 prints the
//!   v1 string, so that is the one reproduced here.

use crate::skill::Skill;

/// The built-in skill's name.
pub const NAME: &str = "customize-opencode";

/// The literal `location` the oracle reports for it.
pub const LOCATION: &str = "<built-in>";

/// `CUSTOMIZE_OPENCODE_SKILL_DESCRIPTION` (`skill/index.ts:33-34`), verbatim.
pub const DESCRIPTION: &str = "Use ONLY when the user is editing or creating Zuno's own configuration: opencode.json, opencode.jsonc, files under .zuno/, or files under ~/.config/zuno/. Also use when creating or fixing Zuno agents, subagents, skills, plugins, MCP servers, or permission rules. Do not use for the user's own application code, or for any project that is not configuring Zuno itself.";

/// The body, byte-identical to
/// `packages/core/src/plugin/skill/customize-opencode.md` at 1.18.13.
pub const CONTENT: &str = include_str!("customize-opencode.md");

/// The built-in skill, as it appears in `Skill.all()`.
#[must_use]
pub fn skill() -> Skill {
    Skill {
        name: NAME.to_string(),
        description: Some(DESCRIPTION.to_string()),
        location: LOCATION.to_string(),
        content: CONTENT.to_string(),
    }
}

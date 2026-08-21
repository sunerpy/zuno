//! `skill` — loading one discovered `SKILL.md` body on demand.
//!
//! # Why a tool at all, when the catalog is already in the prompt
//!
//! [`zuno_catalog::skill::Form::Verbose`] puts every skill's name, description and
//! *location* in the system prompt. A model holding a location can already reach the
//! body with `read`, so this tool is not the only path — it is the *cheap* one. The
//! difference is two round trips and a path the model has to get exactly right,
//! against one call keyed on a name the prompt just gave it. Progressive disclosure
//! only works if the second step is easier than not taking it.
//!
//! # The refusal is the interesting half
//!
//! `read` on a wrong path answers "no such file", which tells a model nothing about
//! what it *should* have asked for. An unknown skill name here answers with the
//! names that exist, so a near-miss (`lark-doc` for `lark-docs`) is self-correcting
//! without a recovery hook — the same property [`crate::task`]'s refusals are built
//! for. The list is bounded, because a refusal that pastes 189 names back is a
//! refusal the model has to pay for.
//!
//! # Why the body is returned whole
//!
//! [`crate::output_policy`] exists because a tool can produce more output than
//! anyone asked for. That is not this tool: the body is exactly the artefact the
//! model named, and a truncated `SKILL.md` is a skill that misbehaves in a way
//! neither the user nor the model can see. Discovery already read every body into
//! memory ([`zuno_catalog::skill::load`]), so the size is knowable in advance and
//! belongs to whoever installed the skill.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use zuno_catalog::skill::Skills;
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolOutput, TypedTool};

/// The id the model calls, and the registry slot it fills
/// ([`crate::registry::BuiltinSlot::Skill`]).
pub const WIRE_ID: &str = "skill";

/// How many names an unknown-skill refusal lists before it stops.
pub const SUGGESTION_LIMIT: usize = 40;

/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/skill.txt");

/// Arguments for one skill load.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillParams {
    /// The skill name, exactly as `<available_skills>` lists it.
    pub name: String,
}

/// Why a skill could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum SkillRejection {
    #[error(
        "Unknown skill `{requested}`. Set `name` to one of the skills \
         <available_skills> lists: {names}{more}"
    )]
    Unknown {
        requested: String,
        names: String,
        more: String,
    },

    #[error(
        "No skills are available in this session, so `{WIRE_ID}` has nothing to \
         load. Install a skill under `.agents/skills/` or add a `skills.paths[]` \
         entry to your configuration."
    )]
    Empty,

    #[error(
        "Skill `{requested}` was discovered but its body is empty, so there is \
         nothing to follow. Check {location}."
    )]
    Bodyless { requested: String, location: String },
}

/// On-demand access to the skills discovery already loaded.
///
/// Holds the loaded [`Skills`] rather than a path root, because
/// [`zuno_catalog::skill::load`] already reads every body into memory and re-reading
/// per call would let the tool answer from a different set than the prompt described.
pub struct SkillTool {
    skills: Arc<Skills>,
}

impl SkillTool {
    /// A tool answering from `skills`.
    #[must_use]
    pub const fn new(skills: Arc<Skills>) -> Self {
        Self { skills }
    }
}

#[async_trait]
impl TypedTool for SkillTool {
    type Params = SkillParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: SkillParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(skill) = self.skills.get(&params.name) else {
            return Err(reject(unknown(&self.skills, &params.name)));
        };
        if skill.content.trim().is_empty() {
            return Err(reject(SkillRejection::Bodyless {
                requested: skill.name.clone(),
                location: skill.location.clone(),
            }));
        }
        Ok(
            ToolOutput::text(format!("Skill: {}", skill.name), skill.content.clone())
                .with_metadata("name", skill.name.clone())
                .with_metadata("location", skill.location.clone()),
        )
    }
}

fn unknown(skills: &Skills, requested: &str) -> SkillRejection {
    let mut available: Vec<&str> = skills
        .all()
        .iter()
        .filter(|skill| skill.description.is_some())
        .map(|skill| skill.name.as_str())
        .collect();
    if available.is_empty() {
        return SkillRejection::Empty;
    }
    available.sort_unstable();
    let total = available.len();
    available.truncate(SUGGESTION_LIMIT);
    SkillRejection::Unknown {
        requested: requested.to_owned(),
        names: available.join(", "),
        more: if total > SUGGESTION_LIMIT {
            format!(" (and {} more)", total - SUGGESTION_LIMIT)
        } else {
            String::new()
        },
    }
}

fn reject(rejection: SkillRejection) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(rejection),
    }
}

#[cfg(test)]
mod tests;

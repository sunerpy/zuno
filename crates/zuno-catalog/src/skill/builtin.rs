//! First-party Skills compiled into the binary.
//!
//! The orchestration crate owns immutable resources and provenance. This module
//! adapts those descriptors to the catalog's source-identity and visibility model.

use crate::skill::Skill;

/// The customization Skill's stable name, retained for callers testing collisions.
pub const NAME: &str = zuno_orchestration::SKILLS[0].name;

/// Prefix shared by every first-party catalog source.
pub const LOCATION_PREFIX: &str = "builtin://zuno-orchestration/";

/// Every first-party descriptor in stable presentation order.
pub fn descriptors() -> &'static [zuno_orchestration::BuiltinSkillDescriptor] {
    zuno_orchestration::skills()
}

/// First-party names in stable presentation order.
pub fn names() -> impl ExactSizeIterator<Item = &'static str> {
    descriptors().iter().map(|descriptor| descriptor.name)
}

/// Adapt all first-party descriptors into catalog entries.
#[must_use]
pub fn skills() -> impl ExactSizeIterator<Item = Skill> {
    descriptors().iter().map(to_skill)
}

/// Adapt one named first-party descriptor into a catalog entry.
#[must_use]
pub fn skill(name: &str) -> Option<Skill> {
    zuno_orchestration::skill(name).map(to_skill)
}

/// Descriptor owning `location`, when it is a first-party source.
#[must_use]
pub fn descriptor_by_location(
    location: &str,
) -> Option<&'static zuno_orchestration::BuiltinSkillDescriptor> {
    descriptors()
        .iter()
        .find(|descriptor| descriptor.location == location)
}

/// Whether `location` belongs to the first-party pack.
#[must_use]
pub fn is_location(location: &str) -> bool {
    location.starts_with(LOCATION_PREFIX) && descriptor_by_location(location).is_some()
}

/// Whether one source may be advertised to `profile` under its explicit tool list.
///
/// `None` means the profile did not restrict tools, so the runtime's canonical
/// native surface remains available. External Skills are not governed by this pack.
#[must_use]
pub fn visible_to(location: &str, profile: &str, tools: Option<&[String]>) -> bool {
    let Some(descriptor) = descriptor_by_location(location) else {
        return true;
    };
    descriptor.allowed_profiles.contains(&profile)
        && tools.is_none_or(|tools| {
            descriptor
                .required_tools
                .iter()
                .all(|required| tools.iter().any(|tool| tool == required))
        })
}

fn to_skill(descriptor: &zuno_orchestration::BuiltinSkillDescriptor) -> Skill {
    Skill::embedded(
        descriptor.name,
        Some(descriptor.description.to_owned()),
        descriptor.location,
        descriptor.content,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_requires_an_allowed_profile_and_every_explicit_tool() {
        let deepwork = zuno_orchestration::skill("deepwork").expect("deepwork descriptor");
        assert!(visible_to(deepwork.location, "orchestrator", None));
        assert!(!visible_to(deepwork.location, "explorer", None));

        let incomplete = [
            "plan_get".to_owned(),
            "plan_update".to_owned(),
            "todo_get".to_owned(),
        ];
        assert!(!visible_to(
            deepwork.location,
            "orchestrator",
            Some(&incomplete)
        ));
        let complete = deepwork
            .required_tools
            .iter()
            .map(|tool| (*tool).to_owned())
            .collect::<Vec<_>>();
        assert!(visible_to(
            deepwork.location,
            "orchestrator",
            Some(&complete)
        ));
    }

    #[test]
    fn external_sources_are_not_governed_by_first_party_metadata() {
        assert!(visible_to(
            "/project/.zuno/skills/custom/SKILL.md",
            "custom-agent",
            Some(&[])
        ));
        assert!(!is_location("builtin://zuno-orchestration/unknown"));
    }
}

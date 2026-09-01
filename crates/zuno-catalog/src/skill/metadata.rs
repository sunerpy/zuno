//! Optional Skill sidecar metadata.
//!
//! `agents/openai.yaml` is the shared compatibility surface. Zuno reads only
//! fields that have an active runtime consumer and ignores unknown fields so a
//! sidecar can also serve other Agent Skills hosts. `agents/zuno.yaml` supports
//! the same fields and overlays the shared file field-by-field, plus the native
//! `policy.exposure` catalog control.

use std::io;
use std::path::Path;

use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlLoader};

use super::{SkillExposure, SkillWarning, SkillWarningKind};

const METADATA_DIRECTORY: &str = "agents";
const OPENAI_METADATA: &str = "openai.yaml";
const ZUNO_METADATA: &str = "zuno.yaml";

/// Effective metadata collected from all recognized sidecars.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SkillMetadata {
    pub display_name: Option<String>,
    pub short_description: Option<String>,
    pub allow_implicit_invocation: Option<bool>,
    pub exposure: Option<SkillExposure>,
    pub sources: Vec<String>,
}

impl SkillMetadata {
    fn overlay(&mut self, other: Self) {
        if other.display_name.is_some() {
            self.display_name = other.display_name;
        }
        if other.short_description.is_some() {
            self.short_description = other.short_description;
        }
        if other.allow_implicit_invocation.is_some() {
            self.allow_implicit_invocation = other.allow_implicit_invocation;
        }
        if other.exposure.is_some() {
            self.exposure = other.exposure;
        }
        self.sources.extend(other.sources);
    }

    pub fn resolved_exposure(&self) -> SkillExposure {
        self.exposure.unwrap_or_else(|| {
            if self.allow_implicit_invocation == Some(false) {
                SkillExposure::Explicit
            } else {
                SkillExposure::Index
            }
        })
    }
}

/// Read the shared sidecar and then the Zuno-native override.
pub(super) async fn load(skill_path: &Path) -> (SkillMetadata, Vec<SkillWarning>) {
    let Some(root) = skill_path.parent() else {
        return (SkillMetadata::default(), Vec::new());
    };
    let mut metadata = SkillMetadata::default();
    let mut warnings = Vec::new();

    for (file_name, native) in [(OPENAI_METADATA, false), (ZUNO_METADATA, true)] {
        let path = root.join(METADATA_DIRECTORY).join(file_name);
        match tokio::fs::read_to_string(&path).await {
            Ok(source) => match parse(&source, native) {
                Ok(mut sidecar) => {
                    sidecar.sources.push(path.to_string_lossy().into_owned());
                    metadata.overlay(sidecar);
                }
                Err(detail) => warnings.push(SkillWarning::new(
                    path.to_string_lossy().as_ref(),
                    SkillWarningKind::MetadataMalformed(detail),
                )),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warnings.push(SkillWarning::new(
                path.to_string_lossy().as_ref(),
                SkillWarningKind::MetadataUnreadable(error.kind()),
            )),
        }
    }

    (metadata, warnings)
}

fn parse(source: &str, native: bool) -> Result<SkillMetadata, String> {
    if source
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .all(|line| line.trim().is_empty())
    {
        return Ok(SkillMetadata::default());
    }
    let documents = YamlLoader::load_from_str(source).map_err(|error| error.to_string())?;
    let root = documents
        .first()
        .ok_or_else(|| "metadata document is empty".to_owned())?;
    let Yaml::Hash(root) = root else {
        return Err("metadata root must be a mapping".to_owned());
    };

    let interface = optional_mapping(root, "interface")?;
    let policy = optional_mapping(root, "policy")?;
    let display_name = interface
        .map(|mapping| optional_non_empty_string(mapping, "display_name"))
        .transpose()?
        .flatten();
    let short_description = interface
        .map(|mapping| optional_non_empty_string(mapping, "short_description"))
        .transpose()?
        .flatten();
    let allow_implicit_invocation = policy
        .map(|mapping| optional_bool(mapping, "allow_implicit_invocation"))
        .transpose()?
        .flatten();
    let exposure = if native {
        policy
            .map(|mapping| optional_exposure(mapping, "exposure"))
            .transpose()?
            .flatten()
    } else {
        None
    };

    Ok(SkillMetadata {
        display_name,
        short_description,
        allow_implicit_invocation,
        exposure,
        sources: Vec::new(),
    })
}

fn optional_mapping<'a>(mapping: &'a Hash, key: &str) -> Result<Option<&'a Hash>, String> {
    match mapping.get(&yaml_key(key)) {
        None | Some(Yaml::Null) => Ok(None),
        Some(Yaml::Hash(value)) => Ok(Some(value)),
        Some(_) => Err(format!("`{key}` must be a mapping when present")),
    }
}

fn optional_non_empty_string(mapping: &Hash, key: &str) -> Result<Option<String>, String> {
    match mapping.get(&yaml_key(key)) {
        None | Some(Yaml::Null) => Ok(None),
        Some(Yaml::String(value)) if !value.trim().is_empty() => Ok(Some(value.trim().to_owned())),
        Some(Yaml::String(_)) => Err(format!("`{key}` must not be empty")),
        Some(_) => Err(format!("`{key}` must be a string when present")),
    }
}

fn optional_bool(mapping: &Hash, key: &str) -> Result<Option<bool>, String> {
    match mapping.get(&yaml_key(key)) {
        None | Some(Yaml::Null) => Ok(None),
        Some(Yaml::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("`{key}` must be a boolean when present")),
    }
}

fn optional_exposure(mapping: &Hash, key: &str) -> Result<Option<SkillExposure>, String> {
    let Some(value) = optional_non_empty_string(mapping, key)? else {
        return Ok(None);
    };
    let exposure = match value.as_str() {
        "index" => SkillExposure::Index,
        "search" => SkillExposure::Search,
        "explicit" => SkillExposure::Explicit,
        _ => {
            return Err(format!(
                "`{key}` must be one of `index`, `search`, or `explicit`"
            ));
        }
    };
    Ok(Some(exposure))
}

fn yaml_key(key: &str) -> Yaml {
    Yaml::String(key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_metadata_reads_only_supported_interface_and_policy_fields() {
        let parsed = parse(
            "interface:\n  display_name: Release Helper\n  short_description: Ship Rust CLIs\n  default_prompt: ignored\npolicy:\n  allow_implicit_invocation: false\n",
            false,
        )
        .expect("metadata parses");

        assert_eq!(parsed.display_name.as_deref(), Some("Release Helper"));
        assert_eq!(parsed.short_description.as_deref(), Some("Ship Rust CLIs"));
        assert_eq!(parsed.allow_implicit_invocation, Some(false));
        assert_eq!(parsed.exposure, None);
        assert_eq!(parsed.resolved_exposure(), SkillExposure::Explicit);
    }

    #[test]
    fn native_exposure_is_typed_and_overrides_implicit_policy() {
        let parsed = parse(
            "policy:\n  allow_implicit_invocation: false\n  exposure: search\n",
            true,
        )
        .expect("metadata parses");

        assert_eq!(parsed.exposure, Some(SkillExposure::Search));
        assert_eq!(parsed.resolved_exposure(), SkillExposure::Search);
    }

    #[test]
    fn malformed_supported_fields_are_rejected_without_accepting_coercions() {
        let error = parse("policy:\n  allow_implicit_invocation: no\n", false)
            .expect_err("YAML string is not a boolean");
        assert!(error.contains("must be a boolean"), "{error}");
    }

    #[test]
    fn native_exposure_rejects_unknown_values() {
        let error =
            parse("policy:\n  exposure: hidden\n", true).expect_err("unknown exposure fails");
        assert!(error.contains("index"), "{error}");
        assert!(error.contains("search"), "{error}");
        assert!(error.contains("explicit"), "{error}");
    }

    #[test]
    fn overlays_are_field_wise() {
        let mut shared = parse(
            "interface:\n  display_name: Shared\npolicy:\n  allow_implicit_invocation: false\n",
            false,
        )
        .expect("shared");
        let native = parse(
            "interface:\n  short_description: Native summary\npolicy:\n  allow_implicit_invocation: true\n",
            true,
        )
        .expect("native");
        shared.overlay(native);

        assert_eq!(shared.display_name.as_deref(), Some("Shared"));
        assert_eq!(shared.short_description.as_deref(), Some("Native summary"));
        assert_eq!(shared.allow_implicit_invocation, Some(true));
        assert_eq!(shared.resolved_exposure(), SkillExposure::Index);
    }

    #[test]
    fn metadata_path_constants_are_relative_to_the_skill_directory() {
        let root = std::path::PathBuf::from("/skills/release");
        assert_eq!(
            root.join(METADATA_DIRECTORY).join(OPENAI_METADATA),
            root.join("agents").join("openai.yaml")
        );
        assert_eq!(
            root.join(METADATA_DIRECTORY).join(ZUNO_METADATA),
            root.join("agents").join("zuno.yaml")
        );
    }
}

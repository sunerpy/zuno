//! `skill` — searching metadata, loading instructions, and reading their resources.
//!
//! # Progressive discovery
//!
//! The system prompt carries [`zuno_catalog::skill::Form::Index`], a bounded
//! name/description/source catalog. `list` pages the full metadata set, `search`
//! queries it, `load` reads one exact `SKILL.md` source, and `read_resource`
//! resolves referenced text inside that skill's root. This keeps every skill
//! discoverable without prepending every instruction body to every request.
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
//! # Why reads are paginated
//!
//! A selected document must be read to completion, but one tool response must not
//! monopolize the model context. Large bodies and resources therefore use opaque,
//! content-bound cursors. A changed file invalidates the cursor instead of joining
//! pages from two versions, and each partial response explicitly requires the next
//! page before task action.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use zuno_catalog::skill::{Skill, Skills, locale_compare};
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy, TypedTool};

/// The id the model calls, and the registry slot it fills
/// ([`crate::registry::BuiltinSlot::Skill`]).
pub const WIRE_ID: &str = "skill";

/// How many names an unknown-skill refusal lists before it stops.
pub const SUGGESTION_LIMIT: usize = 40;

/// Number of matches returned when `search.limit` is absent.
pub const DEFAULT_SEARCH_LIMIT: usize = 8;

/// Hard bound on one search response's match count.
pub const MAX_SEARCH_LIMIT: usize = 20;

/// Skills returned by one `list` page.
pub const LIST_PAGE_SIZE: usize = 20;

/// Maximum UTF-8 bytes of one normalized frontmatter description in search output.
pub const SEARCH_DESCRIPTION_MAX_BYTES: usize = 1_200;

/// Maximum size accepted for one skill document or referenced text resource.
pub const RESOURCE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Maximum UTF-8 content placed in one tool response page.
pub const READ_PAGE_CONTENT_BYTES: usize = 60 * 1024;

/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/skill.txt");

/// Search descriptions, load one skill body, or read one referenced text resource.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillParams {
    /// Whether to `list`, `search`, `load`, or `read_resource`.
    action: SkillAction,
    /// A concise task intent or capability query. Required only for `search`.
    #[serde(default)]
    query: Option<String>,
    /// The exact skill name from the index or search results. Required for `load` and
    /// `read_resource`.
    #[serde(default)]
    name: Option<String>,
    /// Exact source locator returned by the catalog, list, or search result. Required
    /// when more than one installed skill declares `name`.
    #[serde(default)]
    source: Option<String>,
    /// A relative text-resource path inside the selected skill. Required only for
    /// `read_resource`.
    #[serde(default)]
    path: Option<String>,
    /// Opaque continuation returned by a previous list or read operation.
    #[serde(default)]
    cursor: Option<String>,
    /// Maximum search matches. Optional for `search`; invalid for other actions.
    #[serde(default)]
    #[schemars(range(min = 1, max = 20))]
    limit: Option<usize>,
}

/// Progressive-discovery operation selected by [`SkillParams`].
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SkillAction {
    // Enumerate discoverable metadata in stable pages.
    List,
    // Query names and bounded descriptions.
    Search,
    // Return one complete `SKILL.md` body with authoritative source context.
    Load,
    // Read one UTF-8 resource relative to the selected skill's root.
    ReadResource,
}

impl SkillAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Search => "search",
            Self::Load => "load",
            Self::ReadResource => "read_resource",
        }
    }
}

impl SkillParams {
    /// Construct a metadata search for same-process callers.
    #[must_use]
    pub fn search(query: impl Into<String>, limit: Option<usize>) -> Self {
        Self {
            action: SkillAction::Search,
            query: Some(query.into()),
            name: None,
            source: None,
            path: None,
            cursor: None,
            limit,
        }
    }

    /// Construct an exact body load for same-process callers.
    #[must_use]
    pub fn load(name: impl Into<String>) -> Self {
        Self {
            action: SkillAction::Load,
            query: None,
            name: Some(name.into()),
            source: None,
            path: None,
            cursor: None,
            limit: None,
        }
    }

    /// Construct a bounded read of one text resource relative to a skill root.
    #[must_use]
    pub fn read_resource(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            action: SkillAction::ReadResource,
            query: None,
            name: Some(name.into()),
            source: None,
            path: Some(path.into()),
            cursor: None,
            limit: None,
        }
    }
}

/// Why a skill could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum SkillRejection {
    #[error(
        "Unknown skill `{requested}`. Use action `search` to find a matching \
         description, or set `name` to one of these indexed skills: {names}{more}"
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

    #[error("Skill name `{requested}` is ambiguous. Retry with one exact `source`: {sources}")]
    Ambiguous { requested: String, sources: String },

    #[error(
        "Skill `{requested}` has no source `{locator}`. Use action `list` or `search` and \
         pass one of its advertised source locators."
    )]
    SourceMismatch { requested: String, locator: String },

    #[error("`skill` action `{action}` received an invalid or stale `cursor`")]
    InvalidCursor { action: &'static str },

    #[error("`skill` search needs a non-empty `query` containing a word or name")]
    EmptySearch,

    #[error("`skill` search `limit` must be between 1 and {maximum}, got {requested}")]
    SearchLimit { requested: usize, maximum: usize },

    #[error("`skill` action `{action}` requires `{field}`")]
    MissingField {
        action: &'static str,
        field: &'static str,
    },

    #[error("`skill` action `{action}` does not accept `{field}`")]
    UnexpectedField {
        action: &'static str,
        field: &'static str,
    },

    #[error(
        "Skill `{requested}` is not backed by a local SKILL.md, so it has no readable \
         resource root ({location})"
    )]
    NoResourceRoot { requested: String, location: String },

    #[error(
        "Skill resource `{path}` for `{requested}` must be a non-empty relative path and \
         cannot contain `.` or `..` components"
    )]
    InvalidResourcePath { requested: String, path: String },

    #[error("Skill resource `{path}` for `{requested}` is unavailable: {detail}")]
    ResourceUnavailable {
        requested: String,
        path: String,
        detail: String,
    },

    #[error("Skill resource `{path}` for `{requested}` resolves outside its skill root")]
    ResourceOutsideRoot { requested: String, path: String },

    #[error("Skill resource `{path}` for `{requested}` is not a regular file")]
    ResourceNotFile { requested: String, path: String },

    #[error(
        "Skill resource `{path}` for `{requested}` is {bytes} bytes, above the \
         {maximum}-byte text-resource limit"
    )]
    ResourceTooLarge {
        requested: String,
        path: String,
        bytes: usize,
        maximum: usize,
    },

    #[error("Skill resource `{path}` for `{requested}` is not UTF-8 text")]
    ResourceNotText { requested: String, path: String },

    #[error(
        "Skill `{requested}` was discovered but its body is empty, so there is \
         nothing to follow. Check {location}."
    )]
    Bodyless { requested: String, location: String },

    #[error("Skill `{requested}` is {bytes} bytes, above the {maximum}-byte document limit")]
    DocumentTooLarge {
        requested: String,
        bytes: usize,
        maximum: usize,
    },

    #[error("Skill `{requested}` could not be read from {location}: {detail}")]
    DocumentUnavailable {
        requested: String,
        location: String,
        detail: String,
    },
}

/// On-demand access to the skills discovery already loaded.
///
/// Holds the catalog metadata and source identities rather than rediscovering the
/// filesystem per call. A selected disk body is intentionally re-read so edits are
/// visible, while the declared name is checked against the catalog entry before any
/// content is returned.
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

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn effect(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn run(&self, params: SkillParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let SkillParams {
            action,
            query,
            name,
            source,
            path,
            cursor,
            limit,
        } = params;
        match action {
            SkillAction::List => {
                reject_unexpected(action, "query", query.as_ref())?;
                reject_unexpected(action, "name", name.as_ref())?;
                reject_unexpected(action, "source", source.as_ref())?;
                reject_unexpected(action, "path", path.as_ref())?;
                reject_unexpected(action, "limit", limit.as_ref())?;
                self.list(cursor.as_deref())
            }
            SkillAction::Search => {
                reject_unexpected(action, "name", name.as_ref())?;
                reject_unexpected(action, "source", source.as_ref())?;
                reject_unexpected(action, "path", path.as_ref())?;
                reject_unexpected(action, "cursor", cursor.as_ref())?;
                let query = required(action, "query", query)?;
                self.search(&query, limit)
            }
            SkillAction::Load => {
                reject_unexpected(action, "query", query.as_ref())?;
                reject_unexpected(action, "path", path.as_ref())?;
                reject_unexpected(action, "limit", limit.as_ref())?;
                let name = required(action, "name", name)?;
                self.load(&name, source.as_deref(), cursor.as_deref()).await
            }
            SkillAction::ReadResource => {
                reject_unexpected(action, "query", query.as_ref())?;
                reject_unexpected(action, "limit", limit.as_ref())?;
                let name = required(action, "name", name)?;
                let path = required(action, "path", path)?;
                self.read_resource(&name, source.as_deref(), &path, cursor.as_deref())
                    .await
            }
        }
    }
}

impl SkillTool {
    async fn load(
        &self,
        name: &str,
        source: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<ToolOutput, ToolError> {
        let skill = resolve_skill(&self.skills, name, source)?;
        let content = skill.read_body().await.map_err(|error| {
            reject(SkillRejection::DocumentUnavailable {
                requested: skill.name.clone(),
                location: skill.location.clone(),
                detail: error.to_string(),
            })
        })?;
        if content.len() > RESOURCE_MAX_BYTES {
            return Err(reject(SkillRejection::DocumentTooLarge {
                requested: skill.name.clone(),
                bytes: content.len(),
                maximum: RESOURCE_MAX_BYTES,
            }));
        }
        if content.trim().is_empty() {
            return Err(reject(SkillRejection::Bodyless {
                requested: skill.name.clone(),
                location: skill.location.clone(),
            }));
        }
        let page = text_page(SkillAction::Load, &skill.location, &content, cursor)?;
        let root = skill.resource_root().map(Path::to_path_buf);
        let mut context = if page.start == 0 {
            vec![
                format!("Loaded skill `{}`.", skill.name),
                format!("Source: `{}`", skill.location),
            ]
        } else {
            vec![
                format!("Continuing skill `{}`.", skill.name),
                format!("Source: `{}`", skill.location),
            ]
        };
        if page.start == 0 {
            match &root {
                Some(root) => {
                    context.push(format!("Resource root: `{}`", root.display()));
                    context.push(
                        "For relative text references, call `skill` with action \
                         `read_resource`, this exact skill name and source, and the referenced \
                         relative path. Use this root directly for assets or scripts; do not \
                         search the filesystem to rediscover this skill."
                            .to_owned(),
                    );
                }
                None => context.push(
                    "This embedded skill has no filesystem resource root; follow its body \
                     directly."
                        .to_owned(),
                ),
            }
        }
        context.push(String::new());
        context.push(if page.start == 0 {
            String::from("--- BEGIN SKILL BODY ---")
        } else {
            String::from("--- CONTINUE SKILL BODY ---")
        });
        context.push(page.contents);
        if page.next_cursor.is_none() {
            context.push(String::from("--- END SKILL BODY ---"));
        }
        if let Some(next) = &page.next_cursor {
            context.push(format!(
                "The skill body is not complete. Call `skill` again with action `load`, \
                 name `{}`, source `{}`, and cursor `{next}` before taking task action.",
                skill.name, skill.location
            ));
        }
        let mut output = ToolOutput::text(format!("Skill: {}", skill.name), context.join("\n"))
            .with_metadata("name", skill.name.clone())
            .with_metadata("source", skill.location.clone())
            .with_metadata("location", skill.location.clone())
            .with_metadata("complete", page.next_cursor.is_none());
        if let Some(root) = root {
            output = output.with_metadata("root", root.to_string_lossy().into_owned());
        }
        if let Some(next) = page.next_cursor {
            output = output.with_metadata("next_cursor", next);
        }
        Ok(output)
    }

    async fn read_resource(
        &self,
        name: &str,
        source: Option<&str>,
        path: &str,
        cursor: Option<&str>,
    ) -> Result<ToolOutput, ToolError> {
        let skill = resolve_skill(&self.skills, name, source)?;
        let requested = path.trim();
        let relative = Path::new(requested);
        if requested.is_empty()
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(reject(SkillRejection::InvalidResourcePath {
                requested: skill.name.clone(),
                path: path.to_owned(),
            }));
        }
        let root = skill
            .resource_root()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                reject(SkillRejection::NoResourceRoot {
                    requested: skill.name.clone(),
                    location: skill.location.clone(),
                })
            })?;
        let canonical_root = canonicalize_resource(skill, requested, &root).await?;
        let candidate = root.join(relative);
        let canonical = canonicalize_resource(skill, requested, &candidate).await?;
        if !canonical.starts_with(&canonical_root) {
            return Err(reject(SkillRejection::ResourceOutsideRoot {
                requested: skill.name.clone(),
                path: requested.to_owned(),
            }));
        }
        let metadata = tokio::fs::metadata(&canonical).await.map_err(|error| {
            reject(SkillRejection::ResourceUnavailable {
                requested: skill.name.clone(),
                path: requested.to_owned(),
                detail: error.to_string(),
            })
        })?;
        if !metadata.is_file() {
            return Err(reject(SkillRejection::ResourceNotFile {
                requested: skill.name.clone(),
                path: requested.to_owned(),
            }));
        }
        if metadata.len() > RESOURCE_MAX_BYTES as u64 {
            return Err(reject(SkillRejection::ResourceTooLarge {
                requested: skill.name.clone(),
                path: requested.to_owned(),
                bytes: usize::try_from(metadata.len()).unwrap_or(usize::MAX),
                maximum: RESOURCE_MAX_BYTES,
            }));
        }
        let bytes = tokio::fs::read(&canonical).await.map_err(|error| {
            reject(SkillRejection::ResourceUnavailable {
                requested: skill.name.clone(),
                path: requested.to_owned(),
                detail: error.to_string(),
            })
        })?;
        if bytes.len() > RESOURCE_MAX_BYTES {
            return Err(reject(SkillRejection::ResourceTooLarge {
                requested: skill.name.clone(),
                path: requested.to_owned(),
                bytes: bytes.len(),
                maximum: RESOURCE_MAX_BYTES,
            }));
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            reject(SkillRejection::ResourceNotText {
                requested: skill.name.clone(),
                path: requested.to_owned(),
            })
        })?;
        let identity = format!("{}::{requested}", skill.location);
        let page = text_page(SkillAction::ReadResource, &identity, &text, cursor)?;
        let mut contents = page.contents;
        if let Some(next) = &page.next_cursor {
            contents.push_str(&format!(
                "\n\nResource is not complete. Continue with action `read_resource`, name \
                 `{}`, source `{}`, path `{requested}`, and cursor `{next}`.",
                skill.name, skill.location
            ));
        }
        let mut output = ToolOutput::text(
            format!("Skill resource: {}/{}", skill.name, requested),
            contents,
        )
        .with_metadata("name", skill.name.clone())
        .with_metadata("source", skill.location.clone())
        .with_metadata("root", canonical_root.to_string_lossy().into_owned())
        .with_metadata("path", canonical.to_string_lossy().into_owned())
        .with_metadata("resource", requested.to_owned())
        .with_metadata("complete", page.next_cursor.is_none());
        if let Some(next) = page.next_cursor {
            output = output.with_metadata("next_cursor", next);
        }
        Ok(output)
    }

    fn search(&self, query: &str, limit: Option<usize>) -> Result<ToolOutput, ToolError> {
        let query = query.trim();
        let terms = search_terms(query);
        if query.is_empty() || terms.is_empty() {
            return Err(reject(SkillRejection::EmptySearch));
        }
        let limit = limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
        if !(1..=MAX_SEARCH_LIMIT).contains(&limit) {
            return Err(reject(SkillRejection::SearchLimit {
                requested: limit,
                maximum: MAX_SEARCH_LIMIT,
            }));
        }

        let described = self
            .skills
            .all()
            .iter()
            .filter(|skill| skill.description.is_some())
            .collect::<Vec<_>>();
        if described.is_empty() {
            return Err(reject(SkillRejection::Empty));
        }

        let query_folded = query.to_lowercase();
        let mut matches = described
            .iter()
            .filter_map(|skill| {
                let score = search_score(skill, &query_folded, &terms);
                (score > 0).then_some((score, *skill))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| locale_compare(&left.1.name, &right.1.name))
        });

        let matched = matches.len();
        matches.truncate(limit);
        let shown = matches.len();
        let output = if matches.is_empty() {
            format!(
                "No skill description matched `{query}` among {} discoverable skills. \
                 Search again with a broader capability or exact skill-name fragment.",
                described.len()
            )
        } else {
            let mut lines = vec![format!(
                "Skill matches for `{query}` ({shown} shown, {matched} matched, {} available):",
                described.len()
            )];
            lines.extend(matches.iter().map(|(_, skill)| {
                format!(
                    "- {}: {} (source: `{}`)",
                    skill.name,
                    display_description(skill.description.as_deref().unwrap_or_default()),
                    skill.location
                )
            }));
            lines.push(
                "Load the selected instructions with action `load`, its exact `name`, and \
                 the advertised `source`; do not treat a description as the skill body."
                    .to_owned(),
            );
            lines.join("\n")
        };

        Ok(ToolOutput::text(format!("Skill search: {query}"), output)
            .with_metadata("query", query.to_owned())
            .with_metadata("shown", count_value(shown))
            .with_metadata("matched", count_value(matched))
            .with_metadata("available", count_value(described.len())))
    }

    fn list(&self, cursor: Option<&str>) -> Result<ToolOutput, ToolError> {
        let listed = self
            .skills
            .sorted()
            .into_iter()
            .filter(|skill| skill.description.is_some())
            .collect::<Vec<_>>();
        if listed.is_empty() {
            return Err(reject(SkillRejection::Empty));
        }
        let identity = listed
            .iter()
            .map(|skill| {
                format!(
                    "{}\0{}\0{}",
                    skill.name,
                    skill.location,
                    skill.description.as_deref().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let start = cursor_offset(
            SkillAction::List,
            &identity,
            &identity,
            cursor,
            listed.len(),
        )?;
        let end = start.saturating_add(LIST_PAGE_SIZE).min(listed.len());
        let mut lines = vec![format!(
            "Available skills {}-{} of {}:",
            start.saturating_add(1),
            end,
            listed.len()
        )];
        lines.extend(listed[start..end].iter().map(|skill| {
            format!(
                "- {}: {} (source: `{}`)",
                skill.name,
                display_description(skill.description.as_deref().unwrap_or_default()),
                skill.location
            )
        }));
        let next_cursor = (end < listed.len())
            .then(|| encode_cursor(SkillAction::List, end, &identity, &identity));
        if let Some(next) = &next_cursor {
            lines.push(format!(
                "Continue with action `list` and cursor `{next}` for the next page."
            ));
        }
        let mut output = ToolOutput::text("Skills", lines.join("\n"))
            .with_metadata("shown", count_value(end.saturating_sub(start)))
            .with_metadata("available", count_value(listed.len()));
        if let Some(next) = next_cursor {
            output = output.with_metadata("next_cursor", next);
        }
        Ok(output)
    }
}

fn resolve_skill<'a>(
    skills: &'a Skills,
    name: &str,
    source: Option<&str>,
) -> Result<&'a Skill, ToolError> {
    if let Some(source) = source {
        return skills
            .by_source(source)
            .filter(|skill| skill.name == name)
            .ok_or_else(|| {
                reject(SkillRejection::SourceMismatch {
                    requested: name.to_owned(),
                    locator: source.to_owned(),
                })
            });
    }
    let matches = skills.named(name);
    match matches.as_slice() {
        [skill] => Ok(*skill),
        [] => Err(reject(unknown(skills, name))),
        many => Err(reject(SkillRejection::Ambiguous {
            requested: name.to_owned(),
            sources: many
                .iter()
                .map(|skill| format!("`{}`", skill.location))
                .collect::<Vec<_>>()
                .join(", "),
        })),
    }
}

struct TextPage {
    start: usize,
    contents: String,
    next_cursor: Option<String>,
}

fn text_page(
    action: SkillAction,
    identity: &str,
    contents: &str,
    cursor: Option<&str>,
) -> Result<TextPage, ToolError> {
    let start = cursor_offset(action, identity, contents, cursor, contents.len())?;
    if !contents.is_char_boundary(start) {
        return Err(reject(SkillRejection::InvalidCursor {
            action: action.as_str(),
        }));
    }
    let mut end = start
        .saturating_add(READ_PAGE_CONTENT_BYTES)
        .min(contents.len());
    while end > start && !contents.is_char_boundary(end) {
        end -= 1;
    }
    let next_cursor =
        (end < contents.len()).then(|| encode_cursor(action, end, identity, contents));
    Ok(TextPage {
        start,
        contents: contents[start..end].to_owned(),
        next_cursor,
    })
}

fn cursor_offset(
    action: SkillAction,
    identity: &str,
    contents: &str,
    cursor: Option<&str>,
    maximum: usize,
) -> Result<usize, ToolError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let mut parts = cursor.split('|');
    let valid_version = parts.next() == Some("v1");
    let valid_action = parts.next() == Some(action.as_str());
    let offset = parts.next().and_then(|value| value.parse::<usize>().ok());
    let digest = parts.next();
    let no_tail = parts.next().is_none();
    let expected = cursor_digest(action, identity, contents);
    match (valid_version, valid_action, offset, digest, no_tail) {
        (true, true, Some(offset), Some(digest), true)
            if offset <= maximum && digest == expected =>
        {
            Ok(offset)
        }
        _ => Err(reject(SkillRejection::InvalidCursor {
            action: action.as_str(),
        })),
    }
}

fn encode_cursor(action: SkillAction, offset: usize, identity: &str, contents: &str) -> String {
    format!(
        "v1|{}|{offset}|{}",
        action.as_str(),
        cursor_digest(action, identity, contents)
    )
}

fn cursor_digest(action: SkillAction, identity: &str, contents: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(action.as_str().as_bytes());
    digest.update([0]);
    digest.update(identity.as_bytes());
    digest.update([0]);
    digest.update(contents.as_bytes());
    let encoded = hex::encode(digest.finalize());
    encoded[..16].to_owned()
}

async fn canonicalize_resource(
    skill: &Skill,
    requested: &str,
    path: &Path,
) -> Result<PathBuf, ToolError> {
    tokio::fs::canonicalize(path).await.map_err(|error| {
        reject(SkillRejection::ResourceUnavailable {
            requested: skill.name.clone(),
            path: requested.to_owned(),
            detail: error.to_string(),
        })
    })
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
    available.sort_by(|left, right| locale_compare(left, right));
    available.dedup();
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

fn search_terms(query: &str) -> Vec<String> {
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn search_score(skill: &Skill, query: &str, terms: &[String]) -> usize {
    let name = skill.name.to_lowercase();
    let description = skill
        .description
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();
    let mut score = 0usize;

    if name == query {
        score += 10_000;
    } else if name.contains(query) {
        score += 2_000;
    }
    if description.contains(query) {
        score += 1_000;
    }

    for term in terms {
        if name == *term {
            score += 400;
        } else if name
            .split(|character: char| !character.is_alphanumeric())
            .any(|part| part == term)
        {
            score += 250;
        } else if name.contains(term) {
            score += 150;
        }
        if description.contains(term) {
            score += 50;
        }
    }
    score
}

fn display_description(description: &str) -> String {
    let normalized = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return "(empty description)".to_owned();
    }
    truncate_utf8(&normalized, SEARCH_DESCRIPTION_MAX_BYTES)
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let ellipsis = '…';
    let mut end = maximum.saturating_sub(ellipsis.len_utf8()).min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut truncated = value[..end].trim_end().to_owned();
    truncated.push(ellipsis);
    truncated
}

fn count_value(count: usize) -> serde_json::Value {
    serde_json::Value::from(u64::try_from(count).unwrap_or(u64::MAX))
}

fn required<T>(action: SkillAction, field: &'static str, value: Option<T>) -> Result<T, ToolError> {
    value.ok_or_else(|| {
        reject(SkillRejection::MissingField {
            action: action.as_str(),
            field,
        })
    })
}

fn reject_unexpected<T>(
    action: SkillAction,
    field: &'static str,
    value: Option<&T>,
) -> Result<(), ToolError> {
    if value.is_some() {
        return Err(reject(SkillRejection::UnexpectedField {
            action: action.as_str(),
            field,
        }));
    }
    Ok(())
}

fn reject(rejection: SkillRejection) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(rejection),
    }
}

#[cfg(test)]
mod tests;

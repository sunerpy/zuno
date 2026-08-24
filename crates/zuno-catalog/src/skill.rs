//! Native Zuno skill discovery and model-facing progressive disclosure.
//!
//! A skill is a capability pack whose `SKILL.md` frontmatter advertises a name
//! and optional description. Discovery retains bounded metadata and a stable
//! source locator; the selected body is read only when the model calls the
//! `skill` tool. This keeps the base prompt small and lets disk-backed skills be
//! edited without restarting the process.
//!
//! # Identity and selection
//!
//! Paths are de-duplicated during discovery, but names are not identities.
//! Different sources may intentionally declare the same name, so [`Skills`]
//! preserves each source and plain-name lookup succeeds only when the name is
//! unique. The model receives source locators in the catalog and search results
//! and must provide one when a name is ambiguous.
//!
//! # Discovery and materialization
//!
//! ```text
//! SkillOptions ──discover──▶ SkillSources ──pull──▶ source metadata ──select──▶ body
//! ```
//!
//! [`SkillSources::discover`] is synchronous and never touches the network;
//! [`load`] adds configured remote indexes and reads frontmatter concurrently.
//! [`Skill::read_body`] materializes one selected document and verifies that its
//! declared name still matches the catalog entry. The roots and exact patterns
//! are documented on [`discovery`].
//!
//! # Only two frontmatter keys exist
//!
//! `isSkillFrontmatter` (`:53-59`) checks `typeof data.name === "string"` and
//! `data.description === undefined || typeof data.description === "string"`.
//! Nothing else is read: `license`, `version`, `allowed-tools` and friends are
//! ignored, not rejected. Confirmed against the oracle. A `name` that is a
//! number, a boolean, or null drops the skill; so does a `description` that is
//! present and not a string — including `description:` with no value, which YAML
//! resolves to null.

pub mod builtin;
pub mod discovery;
pub mod frontmatter;
pub mod remote;
pub mod render;
pub mod scan;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use futures::stream::StreamExt;
use serde::Serialize;
use zuno_error::{ConfigError, ConfigIssue};

pub use crate::skill::discovery::{Root, SkillOptions, SkillPath, SkillSources};
pub use crate::skill::render::{
    Budgeted, Form, NO_SKILLS, escape_html, fmt as render, fmt_within as render_within,
    locale_compare,
};

/// How many `SKILL.md` frontmatter records are read at once.
///
/// A bound prevents a large imported catalog from exhausting descriptors while
/// keeping independent sources parallel.
pub const LOCAL_CONCURRENCY: usize = 8;

/// One discovered skill.
///
/// The catalog owns metadata and source identity, not a permanent copy of every
/// instruction body. Files are read after selection; built-in and extension
/// skills retain their embedded document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    /// The `name` from frontmatter. The key skills are addressed by.
    pub name: String,
    /// The `description` from frontmatter. `None` hides the skill from model-facing
    /// discovery forms while leaving it in [`Skills::all`], which is deliberate in
    /// the imported behavior (`:322`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where it came from: an absolute path, or the literal `<built-in>`.
    pub location: String,
    /// How its `SKILL.md` body is materialized.
    #[serde(skip)]
    document: SkillDocument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillDocument {
    File(PathBuf),
    Embedded {
        content: String,
        resource_root: Option<PathBuf>,
    },
}

impl Skill {
    /// Metadata backed by a filesystem `SKILL.md`.
    #[must_use]
    pub fn file(name: String, description: Option<String>, path: PathBuf) -> Self {
        Self {
            name,
            description,
            location: path.to_string_lossy().into_owned(),
            document: SkillDocument::File(path),
        }
    }

    /// Metadata and body owned by a native component.
    #[must_use]
    pub fn embedded(
        name: impl Into<String>,
        description: Option<String>,
        location: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description,
            location: location.into(),
            document: SkillDocument::Embedded {
                content: content.into(),
                resource_root: None,
            },
        }
    }

    /// Embedded instructions whose bundled resources live beside `path`.
    #[must_use]
    pub fn embedded_at_path(
        name: impl Into<String>,
        description: Option<String>,
        path: PathBuf,
        content: impl Into<String>,
    ) -> Self {
        let resource_root = path.parent().map(Path::to_path_buf);
        Self {
            name: name.into(),
            description,
            location: path.to_string_lossy().into_owned(),
            document: SkillDocument::Embedded {
                content: content.into(),
                resource_root,
            },
        }
    }

    /// Directory against which this skill resolves relative resources.
    #[must_use]
    pub fn resource_root(&self) -> Option<&Path> {
        match &self.document {
            SkillDocument::File(path) => path.parent(),
            SkillDocument::Embedded { resource_root, .. } => resource_root.as_deref(),
        }
    }

    /// Read the selected body and verify that the source still declares this skill.
    ///
    /// A file may change after discovery. Body edits are observed immediately;
    /// changing the declared name turns the catalog entry stale instead of letting a
    /// source locator silently select a different package.
    pub async fn read_body(&self) -> Result<String, SkillReadError> {
        match &self.document {
            SkillDocument::Embedded { content, .. } => Ok(content.clone()),
            SkillDocument::File(path) => {
                let source = tokio::fs::read_to_string(path).await.map_err(|error| {
                    SkillReadError::new(
                        self.location.clone(),
                        format!("failed to read the selected SKILL.md: {error}"),
                    )
                })?;
                let document = parse_document(&source).map_err(|rejection| {
                    SkillReadError::new(self.location.clone(), rejection.to_string())
                })?;
                if document.name != self.name {
                    return Err(SkillReadError::new(
                        self.location.clone(),
                        format!(
                            "source identity changed from `{}` to `{}`; refresh the skill catalog",
                            self.name, document.name
                        ),
                    ));
                }
                Ok(document.content)
            }
        }
    }
}

/// A selected skill source could no longer provide the document it advertised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillReadError {
    location: String,
    detail: String,
}

impl SkillReadError {
    fn new(location: String, detail: String) -> Self {
        Self { location, detail }
    }
}

impl fmt::Display for SkillReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to read skill {}: {}", self.location, self.detail)
    }
}

impl std::error::Error for SkillReadError {}

struct ParsedDocument {
    name: String,
    description: Option<String>,
    content: String,
}

/// Why one skill, root, or URL did not make it in.
///
/// Every variant is a case the oracle logs and moves past. None of them fails a
/// load: a broken skill file or an unreachable index must not make the agent
/// unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillWarningKind {
    /// A `skills.paths[]` entry that is not a directory (`skill/index.ts:215`).
    PathNotFound,
    /// A root that could not be traversed.
    ScanFailed(io::ErrorKind),
    /// A `SKILL.md` that could not be read.
    Unreadable(io::ErrorKind),
    /// Frontmatter that could not be parsed even after the sanitize retry.
    Frontmatter(String),
    /// No `name`, or a `name` that is not a string.
    MissingName,
    /// A `description` that is present but not a string.
    InvalidDescription,
    /// `index.json` could not be reached.
    IndexUnreachable(String),
    /// `index.json`, or one of its files, exceeded [`remote::REMOTE_TIMEOUT`].
    IndexTimeout,
    /// `index.json` answered with a non-2xx status.
    IndexStatus(u16),
    /// `index.json` is not a valid index document.
    IndexMalformed(String),
    /// An index entry whose `files` does not list `SKILL.md`
    /// (`discovery.ts:67-72`).
    EntryMissingSkillMd {
        /// The entry's name.
        skill: String,
    },
    /// An index entry file that would be written outside its cache directory.
    /// This port's hardening, not the oracle's behaviour.
    UnsafeIndexPath {
        /// The entry's name.
        skill: String,
        /// The offending file path.
        file: String,
    },
    /// A file listed in an index could not be downloaded.
    DownloadFailed {
        /// What the transport said.
        detail: String,
    },
}

/// A warning, and the file or URL it is about.
///
/// The source is always carried separately from the message so a reporter can
/// group by file without parsing prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillWarning {
    source: String,
    kind: SkillWarningKind,
}

impl SkillWarning {
    /// Build a warning about `source`.
    #[must_use]
    pub fn new(source: impl Into<String>, kind: SkillWarningKind) -> Self {
        Self {
            source: source.into(),
            kind,
        }
    }

    /// The file path or URL this is about.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &SkillWarningKind {
        &self.kind
    }
}

impl fmt::Display for SkillWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            SkillWarningKind::PathNotFound => {
                write!(f, "skill path not found: {}", self.source)
            }
            SkillWarningKind::ScanFailed(kind) => {
                write!(f, "failed to scan skills in {}: {kind}", self.source)
            }
            SkillWarningKind::Unreadable(kind) => {
                write!(f, "failed to read skill {}: {kind}", self.source)
            }
            SkillWarningKind::Frontmatter(detail) => {
                write!(f, "failed to load skill {}: {detail}", self.source)
            }
            SkillWarningKind::MissingName => write!(
                f,
                "skill {} has no string `name` in its frontmatter",
                self.source
            ),
            SkillWarningKind::InvalidDescription => write!(
                f,
                "skill {} has a `description` that is not a string",
                self.source
            ),
            SkillWarningKind::IndexUnreachable(detail) => {
                write!(f, "failed to fetch index {}: {detail}", self.source)
            }
            SkillWarningKind::IndexTimeout => write!(
                f,
                "failed to fetch {}: exceeded {:?}",
                self.source,
                remote::REMOTE_TIMEOUT
            ),
            SkillWarningKind::IndexStatus(status) => {
                write!(f, "failed to fetch index {}: HTTP {status}", self.source)
            }
            SkillWarningKind::IndexMalformed(detail) => {
                write!(f, "failed to decode index {}: {detail}", self.source)
            }
            SkillWarningKind::EntryMissingSkillMd { skill } => write!(
                f,
                "skill entry missing SKILL.md: `{skill}` in {}",
                self.source
            ),
            SkillWarningKind::UnsafeIndexPath { skill, file } => write!(
                f,
                "skill entry `{skill}` lists a file outside its cache directory: {file}"
            ),
            SkillWarningKind::DownloadFailed { detail } => {
                write!(f, "failed to download {}: {detail}", self.source)
            }
        }
    }
}

/// The loaded skill set, source-identified and kept in discovery order.
///
/// Same-named skills remain distinct. Plain-name lookup succeeds only when one
/// source declares that name; callers select an ambiguous skill by the source
/// path advertised in the prompt or search result.
#[derive(Debug, Clone, Default)]
pub struct Skills {
    ordered: Vec<Skill>,
    by_name: HashMap<String, Vec<usize>>,
    by_source: HashMap<String, usize>,
    dirs: Vec<PathBuf>,
    warnings: Vec<SkillWarning>,
}

impl Skills {
    /// Resolve `name` only when it names exactly one source.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        let matches = self.by_name.get(name)?;
        (matches.len() == 1)
            .then(|| self.ordered.get(matches[0]))
            .flatten()
    }

    /// Every source declaring `name`, in discovery order.
    #[must_use]
    pub fn named(&self, name: &str) -> Vec<&Skill> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter_map(|at| self.ordered.get(*at))
            .collect()
    }

    /// Resolve one exact advertised source.
    #[must_use]
    pub fn by_source(&self, source: &str) -> Option<&Skill> {
        self.by_source
            .get(source)
            .and_then(|at| self.ordered.get(*at))
    }

    /// `Skill.all` (`:301-304`), in insertion order.
    #[must_use]
    pub fn all(&self) -> &[Skill] {
        &self.ordered
    }

    /// `Skill.available(undefined)` (`:310-315`): every skill, sorted by name.
    ///
    /// The permission filter the oracle applies when an agent is supplied belongs
    /// to the permission engine and is not implemented here.
    #[must_use]
    pub fn sorted(&self) -> Vec<Skill> {
        let mut list = self.ordered.clone();
        list.sort_by(|left, right| {
            locale_compare(&left.name, &right.name).then_with(|| left.location.cmp(&right.location))
        });
        list
    }

    /// `Skill.dirs` (`:306-308`).
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// Everything that was skipped, and why.
    #[must_use]
    pub fn warnings(&self) -> &[SkillWarning] {
        &self.warnings
    }

    /// Render one of the model-facing forms over [`Skills::all`].
    #[must_use]
    pub fn render(&self, form: Form) -> String {
        render(&self.ordered, form)
    }

    /// [`Self::render`], bounded to `budget` bytes.
    #[must_use]
    pub fn render_within(&self, form: Form, budget: usize) -> Budgeted {
        render_within(&self.ordered, form, budget)
    }

    /// A set assembled from skills some other layer already loaded.
    ///
    /// [`load`] is the production path and this is not a second one: it exists so a
    /// caller holding skills from a non-disk source — or a test that must not touch
    /// the filesystem — gets the same source identity and ambiguity rules as
    /// discovery, rather than reimplementing either.
    #[must_use]
    pub fn from_loaded(skills: impl IntoIterator<Item = Skill>) -> Self {
        let mut set = Self::default();
        for skill in skills {
            set.insert(skill);
        }
        set
    }

    /// Overlay non-disk skills while preserving discovery roots and warnings.
    ///
    /// The same source identity updates in place; a different source cannot shadow
    /// a disk skill merely by reusing its name.
    #[must_use]
    pub fn with_overlay(mut self, skills: impl IntoIterator<Item = Skill>) -> Self {
        for skill in skills {
            self.insert(skill);
        }
        self
    }

    /// Register one source identity.
    fn insert(&mut self, skill: Skill) {
        if let Some(at) = self.by_source.get(&skill.location).copied() {
            let previous_name = self.ordered[at].name.clone();
            self.ordered[at] = skill;
            if previous_name != self.ordered[at].name {
                if let Some(matches) = self.by_name.get_mut(&previous_name) {
                    matches.retain(|candidate| *candidate != at);
                }
                self.by_name
                    .entry(self.ordered[at].name.clone())
                    .or_default()
                    .push(at);
            }
            return;
        }
        let at = self.ordered.len();
        self.by_source.insert(skill.location.clone(), at);
        self.by_name.entry(skill.name.clone()).or_default().push(at);
        self.ordered.push(skill);
    }
}

/// Record one actionable discovery warning.
fn warn(sink: &mut Vec<SkillWarning>, warning: SkillWarning) {
    tracing::warn!(skill.source = %warning.source(), "{warning}");
    sink.push(warning);
}

/// Discover and load every skill.
///
/// The built-in `customize-zuno` is registered before disk discovery. A user's
/// same-named skill remains a distinct source.
///
/// Never fails: an unreadable file, an invalid frontmatter block, or an
/// unreachable `skills.urls[]` entry becomes a [`SkillWarning`].
pub async fn load(options: &SkillOptions) -> Skills {
    let mut sources = SkillSources::discover(options);

    if !sources.urls().is_empty() {
        let cache_root = options.remote_cache_root();
        let urls = sources.urls().to_vec();
        let mut dirs = Vec::new();
        let mut warnings = Vec::new();
        // Sequential per URL: the oracle pulls one configured URL at a time
        // (`skill/index.ts:222-227`) and bounds concurrency *within* a pull.
        for url in urls {
            let pulled = remote::pull(&url, &cache_root).await;
            dirs.extend(pulled.dirs);
            warnings.extend(pulled.warnings);
        }
        sources.extend_remote(&dirs);
        for warning in warnings {
            tracing::warn!(skill.source = %warning.source(), "{warning}");
            sources.push_warning(warning);
        }
    }

    let dirs = sources.dirs();
    let mut skills = Skills {
        dirs,
        warnings: sources.take_warnings(),
        ..Skills::default()
    };
    skills.insert(builtin::skill());

    let paths: Vec<PathBuf> = sources
        .matches()
        .iter()
        .map(|entry| entry.path().to_path_buf())
        .collect();

    let read = futures::stream::iter(paths.into_iter().map(|path| async move {
        let outcome = tokio::fs::read_to_string(&path).await;
        (path, outcome)
    }))
    .buffered(LOCAL_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    for (path, outcome) in read {
        match outcome {
            Ok(source) => match parse_source(&path, &source) {
                Ok(skill) => skills.insert(skill),
                Err(rejection) => warn(
                    &mut skills.warnings,
                    SkillWarning::new(path.to_string_lossy().as_ref(), rejection.into()),
                ),
            },
            Err(error) => warn(
                &mut skills.warnings,
                SkillWarning::new(
                    path.to_string_lossy().as_ref(),
                    SkillWarningKind::Unreadable(error.kind()),
                ),
            ),
        }
    }

    let ambiguous_names = skills
        .by_name
        .values()
        .filter(|matches| matches.len() > 1)
        .count();
    tracing::info!(count = skills.all().len(), ambiguous_names, "skills loaded");
    skills
}

/// Read and validate one `SKILL.md`, with a typed error for the caller that wants
/// to surface the failure rather than skip the file.
///
/// [`load`] uses the same validation but turns every failure into a
/// [`SkillWarning`], because one broken skill must not stop the other 135.
pub fn parse_file(path: &Path) -> Result<Skill, ConfigError> {
    let source = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_source(path, &source).map_err(|kind| config_error(path, kind))
}

/// The only three ways one `SKILL.md` can be rejected.
///
/// A narrower type than [`SkillWarningKind`] on purpose: it makes both mappings
/// below exhaustive with no catch-all arm, so adding a rejection reason cannot
/// silently fall through to a generic message.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rejection {
    Frontmatter(String),
    MissingName,
    InvalidDescription,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frontmatter(detail) => write!(f, "invalid frontmatter: {detail}"),
            Self::MissingName => write!(f, "source no longer declares a string `name`"),
            Self::InvalidDescription => {
                write!(f, "source no longer declares a string `description`")
            }
        }
    }
}

impl From<Rejection> for SkillWarningKind {
    fn from(rejection: Rejection) -> Self {
        match rejection {
            Rejection::Frontmatter(detail) => Self::Frontmatter(detail),
            Rejection::MissingName => Self::MissingName,
            Rejection::InvalidDescription => Self::InvalidDescription,
        }
    }
}

fn config_error(path: &Path, rejection: Rejection) -> ConfigError {
    match rejection {
        Rejection::Frontmatter(detail) => ConfigError::Frontmatter {
            path: path.to_path_buf(),
            source: detail.into(),
        },
        Rejection::MissingName => ConfigError::Invalid {
            path: path.to_path_buf(),
            issues: vec![ConfigIssue::new(
                ["name"],
                "a skill needs a string `name` in its frontmatter",
            )],
        },
        Rejection::InvalidDescription => ConfigError::Invalid {
            path: path.to_path_buf(),
            issues: vec![ConfigIssue::new(
                ["description"],
                "`description` must be a string when present",
            )],
        },
    }
}

/// The validation half of a load, with no I/O.
fn parse_source(path: &Path, source: &str) -> Result<Skill, Rejection> {
    let document = parse_document(source)?;
    Ok(Skill::file(
        document.name,
        document.description,
        path.to_path_buf(),
    ))
}

fn parse_document(source: &str) -> Result<ParsedDocument, Rejection> {
    let document =
        frontmatter::parse(source).map_err(|error| Rejection::Frontmatter(error.to_string()))?;

    let Some(name) = document.name.text() else {
        return Err(Rejection::MissingName);
    };
    if document.description.is_wrong_type() {
        return Err(Rejection::InvalidDescription);
    }

    Ok(ParsedDocument {
        name: name.to_string(),
        description: document.description.text().map(str::to_string),
        content: document.content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(path: &str, source: &str) -> Result<Skill, Rejection> {
        parse_source(Path::new(path), source)
    }

    #[test]
    fn a_valid_skill_catalogs_metadata_without_retaining_the_body() {
        let source = "---\nname: a\ndescription: d\n---\n\n# Body\n";
        let skill = at("/s/SKILL.md", source).expect("loads");
        assert_eq!(skill.name, "a");
        assert_eq!(skill.description.as_deref(), Some("d"));
        assert_eq!(skill.location, "/s/SKILL.md");
        assert!(matches!(skill.document, SkillDocument::File(_)));
        assert_eq!(
            parse_document(source).expect("document parses").content,
            "\n# Body\n"
        );
    }

    #[test]
    fn a_description_less_skill_still_loads() {
        let skill = at("/s/SKILL.md", "---\nname: a\n---\nB\n").expect("loads");
        assert_eq!(skill.description, None);
    }

    #[test]
    fn a_missing_name_is_rejected() {
        assert_eq!(
            at("/s/SKILL.md", "---\ndescription: d\n---\nB\n"),
            Err(Rejection::MissingName)
        );
    }

    #[test]
    fn a_non_string_description_is_rejected() {
        assert_eq!(
            at("/s/SKILL.md", "---\nname: a\ndescription: 42\n---\nB\n"),
            Err(Rejection::InvalidDescription)
        );
    }

    #[test]
    fn the_rejection_message_names_the_file() {
        let warning = SkillWarning::new("/skills/broken/SKILL.md", SkillWarningKind::MissingName);
        assert_eq!(
            warning.to_string(),
            "skill /skills/broken/SKILL.md has no string `name` in its frontmatter"
        );
    }

    #[test]
    fn a_config_error_from_a_missing_name_names_the_file_and_the_key() {
        let error = config_error(Path::new("/skills/broken/SKILL.md"), Rejection::MissingName);
        assert!(
            error.to_string().contains("/skills/broken/SKILL.md"),
            "{error}"
        );
        let ConfigError::Invalid { issues, .. } = &error else {
            panic!("expected Invalid, got {error:?}");
        };
        assert_eq!(issues[0].key_path, vec!["name".to_string()]);
    }

    #[test]
    fn updating_the_same_source_keeps_its_position_without_a_warning() {
        let mut skills = Skills::default();
        skills.insert(Skill::embedded(
            "a",
            Some("first".to_string()),
            "/same/SKILL.md",
            "",
        ));
        skills.insert(Skill::embedded(
            "a",
            Some("second".to_string()),
            "/same/SKILL.md",
            "",
        ));

        assert_eq!(skills.all().len(), 1);
        assert_eq!(
            skills.get("a").expect("present").description.as_deref(),
            Some("second")
        );
        assert!(skills.warnings().is_empty());
    }

    #[test]
    fn same_named_skills_keep_both_source_identities_and_plain_lookup_is_ambiguous() {
        let skills = Skills::from_loaded([
            Skill::embedded("a", Some("first".to_string()), "/first/SKILL.md", ""),
            Skill::embedded("a", Some("second".to_string()), "/second/SKILL.md", ""),
        ]);

        assert_eq!(
            skills.all().len(),
            2,
            "same-name skills are distinct packages, not precedence overrides"
        );
        assert!(
            skills.get("a").is_none(),
            "a plain name must not silently choose one of multiple sources"
        );
    }

    #[test]
    fn source_updates_keep_their_insertion_position() {
        let mut skills = Skills::default();
        for (name, location) in [("a", "/1"), ("b", "/2"), ("renamed", "/1")] {
            skills.insert(Skill::embedded(name, Some("d".to_string()), location, ""));
        }
        let names: Vec<&str> = skills.all().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["renamed", "b"]);
        assert!(skills.get("a").is_none());
        assert_eq!(skills.get("renamed").expect("present").location, "/1");
    }

    #[test]
    fn the_builtin_uses_zuno_identity_and_native_configuration() {
        let mut skills = Skills::default();
        skills.insert(builtin::skill());
        let built_in = skills.get(builtin::NAME).expect("present");
        assert_eq!(built_in.location, "<built-in>");
        assert_eq!(built_in.description.as_deref(), Some(builtin::DESCRIPTION));
        assert!(builtin::DESCRIPTION.contains("Zuno's own configuration"));
        assert!(builtin::DESCRIPTION.contains("files under .zuno/"));
        assert!(!builtin::DESCRIPTION.contains("opencode's own configuration"));
        assert!(builtin::CONTENT.contains("# Customizing Zuno"));
        assert!(builtin::CONTENT.contains(".zuno/agent/"));
        assert!(builtin::CONTENT.contains(&format!("{}.json", zuno_paths::CONFIG_FILE_STEM)));
        assert!(!builtin::CONTENT.contains("opencode.ai/config.json"));
        assert!(!builtin::CONTENT.contains("\"plugin\""));
        assert!(!builtin::CONTENT.contains("opencode.json"));
        assert!(!builtin::CONTENT.contains("opencode.jsonc"));
    }

    #[test]
    fn a_user_skill_does_not_silently_override_the_builtin() {
        let mut skills = Skills::default();
        skills.insert(builtin::skill());
        skills.insert(Skill::embedded(
            builtin::NAME,
            Some("mine".to_string()),
            "/mine/SKILL.md",
            "mine",
        ));
        assert!(skills.get(builtin::NAME).is_none());
        assert_eq!(skills.named(builtin::NAME).len(), 2);
        assert!(skills.warnings().is_empty());
    }

    #[test]
    fn skill_metadata_json_omits_body_and_an_absent_description() {
        let json = serde_json::to_string(&Skill::embedded("a", None, "/a", "secret body"))
            .expect("serializes");
        assert_eq!(json, r#"{"name":"a","location":"/a"}"#);
    }
}

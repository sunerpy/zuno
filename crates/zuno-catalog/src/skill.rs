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
mod metadata;
pub mod remote;
pub mod render;
pub mod scan;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use futures::stream::StreamExt;
use serde::Serialize;
use zuno_config::schema::SkillCatalogExposure;
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

/// How an enabled Skill participates in model-driven discovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillExposure {
    /// Advertise metadata in the initial bounded prompt index and in search.
    #[default]
    Index,
    /// Keep metadata out of the initial prompt while retaining search and list.
    Search,
    /// Permit only explicit selection by exact name/source.
    Explicit,
}

impl SkillExposure {
    const fn is_index(&self) -> bool {
        matches!(self, Self::Index)
    }
}

impl From<SkillCatalogExposure> for SkillExposure {
    fn from(value: SkillCatalogExposure) -> Self {
        match value {
            SkillCatalogExposure::Index => Self::Index,
            SkillCatalogExposure::Search => Self::Search,
            SkillCatalogExposure::Explicit => Self::Explicit,
        }
    }
}

/// One discovered skill.
///
/// The catalog owns metadata and source identity, not a permanent copy of every
/// instruction body. Files are read after selection; built-in and extension
/// skills retain their embedded document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    /// The `name` from frontmatter. The key skills are addressed by.
    pub name: String,
    /// The `description` from frontmatter. Without a sidecar short description,
    /// `None` hides the skill from model-driven discovery while leaving it in
    /// [`Skills::all`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional human-facing name supplied by a recognized sidecar.
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional bounded catalog description supplied by a recognized sidecar.
    #[serde(rename = "shortDescription", skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    /// Effective model-discovery policy after sidecars and path configuration.
    #[serde(skip_serializing_if = "SkillExposure::is_index")]
    pub exposure: SkillExposure,
    /// Sidecars that contributed recognized metadata.
    #[serde(rename = "metadataSources", skip_serializing_if = "Vec::is_empty")]
    pub metadata_sources: Vec<String>,
    /// Where it came from: an absolute path or a stable `builtin://` source.
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
            display_name: None,
            short_description: None,
            exposure: SkillExposure::Index,
            metadata_sources: Vec::new(),
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
            display_name: None,
            short_description: None,
            exposure: SkillExposure::Index,
            metadata_sources: Vec::new(),
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
            display_name: None,
            short_description: None,
            exposure: SkillExposure::Index,
            metadata_sources: Vec::new(),
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

    /// Description used for prompt metadata and search results.
    #[must_use]
    pub fn catalog_description(&self) -> Option<&str> {
        self.short_description
            .as_deref()
            .or(self.description.as_deref())
    }

    /// Human-facing title, falling back to the exact invocation name.
    #[must_use]
    pub fn catalog_display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// Whether this Skill belongs in the bounded initial prompt index.
    #[must_use]
    pub fn is_indexed(&self) -> bool {
        self.exposure == SkillExposure::Index && self.catalog_description().is_some()
    }

    /// Whether this Skill belongs in `skill search` and `skill list`.
    #[must_use]
    pub fn is_searchable(&self) -> bool {
        self.exposure != SkillExposure::Explicit && self.catalog_description().is_some()
    }

    /// Whether model-driven selection requires an explicit exact reference.
    #[must_use]
    pub fn is_explicit_only(&self) -> bool {
        self.exposure == SkillExposure::Explicit
    }

    fn apply_metadata(
        mut self,
        metadata: metadata::SkillMetadata,
        configured_exposure: Option<SkillCatalogExposure>,
    ) -> Self {
        let sidecar_exposure = metadata.resolved_exposure();
        self.display_name = metadata.display_name;
        self.short_description = metadata.short_description;
        self.exposure = configured_exposure
            .map(Into::into)
            .unwrap_or(sidecar_exposure);
        self.metadata_sources = metadata.sources;
        self
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
    /// A recognized sidecar could not be read.
    MetadataUnreadable(io::ErrorKind),
    /// A recognized sidecar contains invalid supported metadata.
    MetadataMalformed(String),
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
            SkillWarningKind::MetadataUnreadable(kind) => {
                write!(f, "failed to read skill metadata {}: {kind}", self.source)
            }
            SkillWarningKind::MetadataMalformed(detail) => {
                write!(f, "failed to load skill metadata {}: {detail}", self.source)
            }
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
    disabled_sources: Vec<String>,
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

    /// Skills that may be exposed directly as `/<skill-name>`.
    ///
    /// A direct slash route must resolve to exactly one source, carry enough
    /// metadata for discovery, and never shadow a real command. Keeping this
    /// rule beside the source indexes makes every client project the same
    /// answer instead of independently guessing from [`Self::all`].
    #[must_use]
    pub fn slash_invokable<I, S>(&self, command_names: I) -> Vec<&Skill>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let command_names = command_names
            .into_iter()
            .map(|name| name.as_ref().to_owned())
            .collect::<HashSet<_>>();
        self.ordered
            .iter()
            .filter(|skill| {
                skill.catalog_description().is_some()
                    && self
                        .by_name
                        .get(&skill.name)
                        .is_some_and(|at| at.len() == 1)
                    && !command_names.contains(&skill.name)
            })
            .collect()
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

    /// Model-searchable Skills, sorted by exact invocation name and source.
    #[must_use]
    pub fn searchable_sorted(&self) -> Vec<Skill> {
        self.sorted()
            .into_iter()
            .filter(Skill::is_searchable)
            .collect()
    }

    /// Number of Skills advertised in the initial prompt index.
    #[must_use]
    pub fn indexed_count(&self) -> usize {
        self.ordered
            .iter()
            .filter(|skill| skill.is_indexed())
            .count()
    }

    /// Number of Skills available through model-driven search.
    #[must_use]
    pub fn searchable_count(&self) -> usize {
        self.ordered
            .iter()
            .filter(|skill| skill.is_searchable())
            .count()
    }

    /// Number of enabled Skills that require an exact explicit reference.
    #[must_use]
    pub fn explicit_count(&self) -> usize {
        self.ordered
            .iter()
            .filter(|skill| skill.is_explicit_only())
            .count()
    }

    /// `Skill.dirs` (`:306-308`).
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// Discovered filesystem sources excluded by `skills.config`.
    #[must_use]
    pub fn disabled_sources(&self) -> &[String] {
        &self.disabled_sources
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

    /// [`Self::render`], bounded to `budget` Unicode scalar values.
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

    /// Keep only entries accepted by `predicate`, preserving roots and warnings.
    ///
    /// Rebuilds both indexes from the retained source identities so ambiguity and
    /// exact-source lookup cannot retain stale positions.
    #[must_use]
    pub fn retaining(mut self, mut predicate: impl FnMut(&Skill) -> bool) -> Self {
        let ordered = std::mem::take(&mut self.ordered);
        self.by_name.clear();
        self.by_source.clear();
        for skill in ordered {
            if predicate(&skill) {
                self.insert(skill);
            }
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
/// First-party Skills are registered before disk discovery. A user's same-named
/// Skill remains a distinct source.
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
        disabled_sources: sources.take_disabled_sources(),
        warnings: sources.take_warnings(),
        ..Skills::default()
    };
    for skill in builtin::skills() {
        skills.insert(skill);
    }

    let paths = sources.matches().to_vec();

    let read = futures::stream::iter(paths.into_iter().map(|entry| async move {
        let path = entry.path().to_path_buf();
        let outcome = tokio::fs::read_to_string(&path).await;
        let (metadata, metadata_warnings) = metadata::load(&path).await;
        (path, entry.exposure(), outcome, metadata, metadata_warnings)
    }))
    .buffered(LOCAL_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    for (path, configured_exposure, outcome, metadata, metadata_warnings) in read {
        for warning in metadata_warnings {
            warn(&mut skills.warnings, warning);
        }
        match outcome {
            Ok(source) => match parse_source(&path, &source) {
                Ok(skill) => {
                    skills.insert(skill.apply_metadata(metadata, configured_exposure));
                }
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
    tracing::info!(
        count = skills.all().len(),
        indexed = skills.indexed_count(),
        searchable = skills.searchable_count(),
        explicit = skills.explicit_count(),
        disabled = skills.disabled_sources().len(),
        ambiguous_names,
        "skills loaded"
    );
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
    fn the_first_party_pack_uses_unique_stable_sources_and_original_bodies() {
        let skills = Skills::from_loaded(builtin::skills());
        assert_eq!(skills.all().len(), zuno_orchestration::SKILLS.len());
        for descriptor in zuno_orchestration::SKILLS {
            let built_in = skills.get(descriptor.name).expect("present");
            assert_eq!(built_in.location, descriptor.location);
            assert_eq!(
                built_in.description.as_deref(),
                Some(descriptor.description)
            );
            assert_eq!(
                futures::executor::block_on(built_in.read_body()).expect("embedded body"),
                descriptor.content
            );
        }
    }

    #[test]
    fn a_user_skill_does_not_silently_override_the_builtin() {
        let mut skills = Skills::from_loaded(builtin::skills());
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
    fn slash_invokable_skills_are_described_unambiguous_and_do_not_shadow_commands() {
        let skills = Skills::from_loaded([
            Skill::embedded(
                "direct",
                Some("directly invokable".to_owned()),
                "/one/direct/SKILL.md",
                "direct",
            ),
            Skill::embedded(
                "duplicate",
                Some("first source".to_owned()),
                "/one/duplicate/SKILL.md",
                "first",
            ),
            Skill::embedded(
                "duplicate",
                Some("second source".to_owned()),
                "/two/duplicate/SKILL.md",
                "second",
            ),
            Skill::embedded("undocumented", None, "/one/undocumented/SKILL.md", "hidden"),
            Skill::embedded(
                "compact",
                Some("collides with a real command".to_owned()),
                "/one/compact/SKILL.md",
                "collision",
            ),
        ]);

        let invokable = skills.slash_invokable(["compact", "goal"]);

        assert_eq!(
            invokable
                .iter()
                .map(|skill| (skill.name.as_str(), skill.location.as_str()))
                .collect::<Vec<_>>(),
            vec![("direct", "/one/direct/SKILL.md")]
        );
    }

    #[test]
    fn retaining_first_party_skills_rebuilds_exact_source_and_name_indexes() {
        let skills = Skills::from_loaded(builtin::skills())
            .retaining(|skill| builtin::visible_to(&skill.location, "plan", None, &[]));
        assert_eq!(
            skills
                .all()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "customize-zuno",
                "develop-zuno",
                "deepwork",
                "codemap",
                "verification-planning",
                "github-delivery",
                "ui-design"
            ]
        );
        for skill in skills.all() {
            assert_eq!(skills.get(&skill.name), Some(skill));
            assert_eq!(skills.by_source(&skill.location), Some(skill));
        }
        assert!(skills.get("worktree").is_none());
    }

    #[test]
    fn skill_metadata_json_omits_body_and_an_absent_description() {
        let json = serde_json::to_string(&Skill::embedded("a", None, "/a", "secret body"))
            .expect("serializes");
        assert_eq!(json, r#"{"name":"a","location":"/a"}"#);
    }
}

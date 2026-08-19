//! Skill discovery across six roots, and the two model-facing render forms.
//!
//! Port of `packages/opencode/src/skill/index.ts` and `skill/discovery.ts`
//! (opencode 1.18.13). A skill is a user-authored capability pack: a `SKILL.md`
//! whose frontmatter carries a `name` and a `description`, and whose body is
//! loaded on demand. The descriptions of every visible skill go into the system
//! prompt of **every** request ([`fmt`]), so this module has two failure modes
//! that are both silent and both expensive: discovering a skill the TypeScript
//! binary would not discover gives the agent a capability the user did not
//! install, and missing one takes a capability away. On a machine with ~120
//! skills installed, either drift is invisible until behaviour changes.
//!
//! # The shape of a load
//!
//! ```text
//! SkillOptions ──discover──▶ SkillSources ──pull──▶ +remote dirs ──read──▶ Skills
//!               (roots 1-5)   (paths, dirs)  (root 6)                (name-keyed)
//! ```
//!
//! [`SkillSources::discover`] is synchronous and never touches the network;
//! [`load`] adds the `skills.urls[]` pull and the file reads. The six roots and
//! their exact patterns are documented on [`discovery`].
//!
//! # Two de-duplications that are not the same thing
//!
//! **By path.** The same absolute path reached through two roots is one match
//! (`skill/index.ts:168`, a `Set<string>`). Nothing is canonicalized, so a
//! symlink alias is *not* collapsed here.
//!
//! **By name.** Two different files claiming the same `name` both load, and the
//! later one wins with a warning (`:125-131`). This is where symlink aliases
//! land: on the surveyed machine, 27 of 136 skills exist twice, once under
//! `~/.claude/skills/x` and once as the `~/.agents/skills/x` the first is a
//! symlink to.
//!
//! # One recorded divergence: the duplicate winner is deterministic here
//!
//! The oracle loads every match with `Effect.forEach(..., { concurrency:
//! "unbounded" })` (`:240-243`) and each load starts with an async file read, so
//! the *order the writes land in* is decided by I/O timing. Measured: three runs
//! of `opencode debug skill` over one fixture with the same `name` under
//! `~/.claude`, `~/.agents` and a config directory reported the `~/.agents` copy
//! once and the config copy twice. The name **set** is stable; the winner is not.
//!
//! This port loads in root order and lets the later root win, which reproduces
//! the oracle's real-tree result for every alias on the surveyed machine
//! (`.agents` beats `.claude`, a config directory beats both) and is
//! reproducible. A racy prompt is worse than a slightly different one.
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
pub use crate::skill::render::{Form, NO_SKILLS, escape_html, fmt as render, locale_compare};

/// How many `SKILL.md` files are read at once.
///
/// The oracle reads them all at once (`skill/index.ts:241`). A bound is what
/// makes the load reproducible; 8 matches `zuno_config::instructions`, which is the
/// same problem.
pub const LOCAL_CONCURRENCY: usize = 8;

/// One loaded skill — the oracle's `Skill.Info` (`skill/index.ts:37-43`).
///
/// Field order is the oracle's, because `opencode debug skill` prints this struct
/// with `JSON.stringify(skills, null, 2)` and the differential test compares the
/// two documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    /// The `name` from frontmatter. The key skills are addressed by.
    pub name: String,
    /// The `description` from frontmatter. `None` hides the skill from both render
    /// forms while leaving it in [`Skills::all`], which is deliberate in the
    /// oracle (`:322`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where it came from: an absolute path, or the literal `<built-in>`.
    pub location: String,
    /// The body after the frontmatter, verbatim.
    pub content: String,
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
    /// A second file claiming a name already taken. The later file wins.
    DuplicateName {
        /// The contested name.
        name: String,
        /// Where the name was already registered.
        existing: String,
    },
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
            SkillWarningKind::DuplicateName { name, existing } => write!(
                f,
                "duplicate skill name `{name}`: {} overrides {existing}",
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

/// The loaded skill set, keyed by name, in insertion order.
///
/// The oracle's state is a JavaScript object (`skill/index.ts:83`), so
/// re-assigning an existing name replaces the value **in place** and keeps the
/// original position. `Skill.all()` returns `Object.values`, so that position is
/// observable; this type reproduces it.
#[derive(Debug, Clone, Default)]
pub struct Skills {
    ordered: Vec<Skill>,
    index: HashMap<String, usize>,
    dirs: Vec<PathBuf>,
    warnings: Vec<SkillWarning>,
}

impl Skills {
    /// `Skill.get` (`skill/index.ts:289-292`).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.index.get(name).and_then(|at| self.ordered.get(*at))
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
        list.sort_by(|left, right| locale_compare(&left.name, &right.name));
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

    /// Render one of the two model-facing forms over [`Skills::all`].
    #[must_use]
    pub fn render(&self, form: Form) -> String {
        render(&self.ordered, form)
    }

    /// `state.skills[name] = info` (`skill/index.ts:125-139`) — register a skill,
    /// warning when it displaces one.
    fn insert(&mut self, skill: Skill) {
        if let Some(at) = self.index.get(&skill.name).copied() {
            let existing = self.ordered[at].location.clone();
            warn(
                &mut self.warnings,
                SkillWarning::new(
                    skill.location.clone(),
                    SkillWarningKind::DuplicateName {
                        name: skill.name.clone(),
                        existing,
                    },
                ),
            );
            self.ordered[at] = skill;
            return;
        }
        self.index.insert(skill.name.clone(), self.ordered.len());
        self.ordered.push(skill);
    }
}

fn warn(sink: &mut Vec<SkillWarning>, warning: SkillWarning) {
    tracing::warn!(skill.source = %warning.source(), "{warning}");
    sink.push(warning);
}

/// Discover and load every skill, in the oracle's root order.
///
/// The built-in `customize-opencode` is registered **before** disk discovery
/// (`skill/index.ts:276-283`), so a user's own skill of that name replaces it and
/// gets a duplicate warning — exactly as the oracle intends.
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

    tracing::info!(count = skills.all().len(), "skills loaded");
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
    let document =
        frontmatter::parse(source).map_err(|error| Rejection::Frontmatter(error.to_string()))?;

    let Some(name) = document.name.text() else {
        return Err(Rejection::MissingName);
    };
    if document.description.is_wrong_type() {
        return Err(Rejection::InvalidDescription);
    }

    Ok(Skill {
        name: name.to_string(),
        description: document.description.text().map(str::to_string),
        location: path.to_string_lossy().into_owned(),
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
    fn a_valid_skill_keeps_its_body_verbatim() {
        let skill = at(
            "/s/SKILL.md",
            "---\nname: a\ndescription: d\n---\n\n# Body\n",
        )
        .expect("loads");
        assert_eq!(skill.name, "a");
        assert_eq!(skill.description.as_deref(), Some("d"));
        assert_eq!(skill.location, "/s/SKILL.md");
        assert_eq!(skill.content, "\n# Body\n");
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
    fn a_duplicate_name_warns_once_and_the_later_file_wins() {
        let mut skills = Skills::default();
        skills.insert(Skill {
            name: "a".to_string(),
            description: Some("first".to_string()),
            location: "/first/SKILL.md".to_string(),
            content: String::new(),
        });
        skills.insert(Skill {
            name: "a".to_string(),
            description: Some("second".to_string()),
            location: "/second/SKILL.md".to_string(),
            content: String::new(),
        });

        assert_eq!(skills.all().len(), 1);
        assert_eq!(
            skills.get("a").expect("present").location,
            "/second/SKILL.md"
        );
        assert_eq!(skills.warnings().len(), 1);
        assert_eq!(
            skills.warnings()[0].kind(),
            &SkillWarningKind::DuplicateName {
                name: "a".to_string(),
                existing: "/first/SKILL.md".to_string(),
            }
        );
    }

    #[test]
    fn a_replaced_skill_keeps_its_insertion_position() {
        let mut skills = Skills::default();
        for (name, location) in [("a", "/1"), ("b", "/2"), ("a", "/3")] {
            skills.insert(Skill {
                name: name.to_string(),
                description: Some("d".to_string()),
                location: location.to_string(),
                content: String::new(),
            });
        }
        let names: Vec<&str> = skills.all().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(skills.get("a").expect("present").location, "/3");
    }

    #[test]
    fn the_builtin_uses_zuno_identity_and_keeps_the_compatibility_config_filename() {
        let mut skills = Skills::default();
        skills.insert(builtin::skill());
        let built_in = skills.get(builtin::NAME).expect("present");
        assert_eq!(built_in.location, "<built-in>");
        assert_eq!(built_in.description.as_deref(), Some(builtin::DESCRIPTION));
        assert!(builtin::DESCRIPTION.contains("Zuno's own configuration"));
        assert!(builtin::DESCRIPTION.contains("files under .zuno/"));
        assert!(!builtin::DESCRIPTION.contains("opencode's own configuration"));
        assert!(built_in.content.contains("# Customizing Zuno"));
        assert!(built_in.content.contains(".zuno/agent/"));
        assert!(
            built_in
                .content
                .contains(&format!("{}.json", zuno_paths::CONFIG_FILE_STEM))
        );
        // The skill is what a model reads before writing config, so a stale
        // filename here teaches the old name to every session. `opencode.ai` is
        // still the published schema URL and `@opencode-ai/plugin` is the plugin
        // ABI, so only the *filename* spellings are forbidden.
        for stale in [
            &format!("{}.json", zuno_paths::LEGACY_CONFIG_FILE_STEM),
            &format!("{}.jsonc", zuno_paths::LEGACY_CONFIG_FILE_STEM),
        ] {
            assert!(
                !built_in.content.contains(stale.as_str()),
                "the built-in customization skill still names `{stale}`, which Zuno no longer \
                 reads; a model following it would write a file that is rejected at startup"
            );
        }
    }

    #[test]
    fn a_user_skill_overrides_the_builtin() {
        let mut skills = Skills::default();
        skills.insert(builtin::skill());
        skills.insert(Skill {
            name: builtin::NAME.to_string(),
            description: Some("mine".to_string()),
            location: "/mine/SKILL.md".to_string(),
            content: "mine".to_string(),
        });
        assert_eq!(
            skills.get(builtin::NAME).expect("present").location,
            "/mine/SKILL.md"
        );
        assert_eq!(skills.warnings().len(), 1);
    }

    #[test]
    fn debug_skill_json_omits_an_absent_description() {
        let json = serde_json::to_string(&Skill {
            name: "a".to_string(),
            description: None,
            location: "/a".to_string(),
            content: "b".to_string(),
        })
        .expect("serializes");
        assert_eq!(json, r#"{"name":"a","location":"/a","content":"b"}"#);
    }
}

use crate::{CodeGraphIndexSnapshot, EvidenceAnchor, ReviewSourceSnapshot};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SourceProbeError {
    #[error("review repository root `{path}` is unavailable: {source}")]
    Root {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("review source command `{command}` failed: {detail}")]
    Command { command: String, detail: String },
    #[error("review scope path `{0}` must be repository-relative and contain no parent traversal")]
    InvalidScope(String),
    #[error("review evidence path `{0}` is outside the repository")]
    OutsideRepository(String),
    #[error("review evidence path `{path}` could not be read: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("review evidence path `{path}` ends at line {end} before it starts at line {start}")]
    InvertedRange { path: String, start: u32, end: u32 },
    #[error("review evidence path `{path}` starts at line {start}, past its {lines} line(s)")]
    RangeOutsideFile {
        path: String,
        start: u32,
        lines: usize,
    },
}

pub trait ReviewSourceProbe: Send + Sync + 'static {
    fn capture(
        &self,
        scope_paths: &[String],
        at_ms: i64,
    ) -> Result<ReviewSourceSnapshot, SourceProbeError>;

    fn anchor(&self, requested: &EvidenceAnchor) -> Result<EvidenceAnchor, SourceProbeError>;
}

#[derive(Debug, Clone)]
pub struct RepositoryReviewSourceProbe {
    root: PathBuf,
}

impl RepositoryReviewSourceProbe {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn canonical_root(&self) -> Result<PathBuf, SourceProbeError> {
        self.root
            .canonicalize()
            .map_err(|source| SourceProbeError::Root {
                path: self.root.clone(),
                source,
            })
    }

    fn git(&self, arguments: &[&str], scope_paths: &[String]) -> Result<Output, SourceProbeError> {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.root).args(arguments);
        if !scope_paths.is_empty() {
            command.arg("--");
            command.args(scope_paths);
        }
        command
            .output()
            .map_err(|source| SourceProbeError::Command {
                command: format!("git {}", arguments.join(" ")),
                detail: source.to_string(),
            })
    }

    fn successful(
        &self,
        arguments: &[&str],
        scope_paths: &[String],
    ) -> Result<Vec<u8>, SourceProbeError> {
        let output = self.git(arguments, scope_paths)?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(SourceProbeError::Command {
            command: format!("git {}", arguments.join(" ")),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    fn codegraph(&self) -> CodeGraphIndexSnapshot {
        let output = Command::new("codegraph")
            .arg("status")
            .arg(&self.root)
            .arg("--json")
            .output();
        let Ok(output) = output else {
            return unavailable_codegraph();
        };
        if !output.status.success() {
            return unavailable_codegraph();
        }
        let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
            return unavailable_codegraph();
        };
        let pending = value.get("pendingChanges").and_then(Value::as_object);
        let index = value.get("index").and_then(Value::as_object);
        CodeGraphIndexSnapshot {
            initialized: value
                .get("initialized")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            extraction_status: value
                .get("extractionStatus")
                .and_then(Value::as_str)
                .unwrap_or("unavailable")
                .to_owned(),
            built_with_extraction_version: index
                .and_then(|index| index.get("builtWithExtractionVersion"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default(),
            current_extraction_version: index
                .and_then(|index| index.get("currentExtractionVersion"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default(),
            pending_added: pending_count(pending, "added"),
            pending_modified: pending_count(pending, "modified"),
            pending_removed: pending_count(pending, "removed"),
            worktree_mismatch: value.get("worktreeMismatch").and_then(|value| {
                if value.is_null() {
                    None
                } else {
                    Some(value.to_string())
                }
            }),
            last_indexed_ms: None,
        }
    }

    fn validate_scope(scope_paths: &[String]) -> Result<(), SourceProbeError> {
        for path in scope_paths {
            let path_ref = Path::new(path);
            if path_ref.is_absolute()
                || path_ref.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err(SourceProbeError::InvalidScope(path.clone()));
            }
        }
        Ok(())
    }
}

impl ReviewSourceProbe for RepositoryReviewSourceProbe {
    fn capture(
        &self,
        scope_paths: &[String],
        at_ms: i64,
    ) -> Result<ReviewSourceSnapshot, SourceProbeError> {
        Self::validate_scope(scope_paths)?;
        let root = self.canonical_root()?;
        let head = self.successful(&["rev-parse", "HEAD"], &[])?;
        let head_sha = String::from_utf8_lossy(&head).trim().to_owned();
        let branch_output = self.git(&["symbolic-ref", "--short", "-q", "HEAD"], &[])?;
        let branch = branch_output.status.success().then(|| {
            String::from_utf8_lossy(&branch_output.stdout)
                .trim()
                .to_owned()
        });
        let diff = self.successful(&["diff", "--binary", "--no-ext-diff", "HEAD"], scope_paths)?;
        let untracked = self.successful(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            scope_paths,
        )?;
        let mut digest = Sha256::new();
        digest.update(head_sha.as_bytes());
        digest.update([0]);
        digest.update(&diff);
        digest.update([0]);
        digest.update(&untracked);
        for path in nul_paths(&untracked) {
            let bytes =
                std::fs::read(root.join(&path)).map_err(|source| SourceProbeError::Read {
                    path: path.clone(),
                    source,
                })?;
            digest.update(path.as_bytes());
            digest.update([0]);
            digest.update(bytes);
            digest.update([0]);
        }
        Ok(ReviewSourceSnapshot {
            id: format!("rsnap_{}", Uuid::now_v7().simple()),
            repository_root: root.to_string_lossy().into_owned(),
            head_sha,
            branch,
            worktree_path: root.to_string_lossy().into_owned(),
            dirty: !diff.is_empty() || !untracked.is_empty(),
            worktree_digest: format!("sha256:{}", hex::encode(digest.finalize())),
            scope_paths: scope_paths.to_vec(),
            codegraph: self.codegraph(),
            captured_at_ms: at_ms,
        })
    }

    fn anchor(&self, requested: &EvidenceAnchor) -> Result<EvidenceAnchor, SourceProbeError> {
        let root = self.canonical_root()?;
        let relative = Path::new(requested.path.trim());
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(SourceProbeError::OutsideRepository(requested.path.clone()));
        }
        let candidate = root.join(relative);
        let canonical = candidate
            .canonicalize()
            .map_err(|source| SourceProbeError::Read {
                path: requested.path.clone(),
                source,
            })?;
        if !canonical.starts_with(&root) {
            return Err(SourceProbeError::OutsideRepository(requested.path.clone()));
        }
        let bytes = std::fs::read(&canonical).map_err(|source| SourceProbeError::Read {
            path: requested.path.clone(),
            source,
        })?;
        if let (Some(start), Some(end)) = (requested.start_line, requested.end_line)
            && end < start
        {
            return Err(SourceProbeError::InvertedRange {
                path: requested.path.clone(),
                start,
                end,
            });
        }
        if let Some(start) = requested.start_line {
            let lines = bytes.iter().filter(|byte| **byte == b'\n').count()
                + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
            if start == 0 || usize::try_from(start).unwrap_or(usize::MAX) > lines {
                return Err(SourceProbeError::RangeOutsideFile {
                    path: requested.path.clone(),
                    start,
                    lines,
                });
            }
        }
        let mut anchored = requested.clone();
        anchored.path = relative.to_string_lossy().replace('\\', "/");
        anchored.content_digest = Some(format!("sha256:{}", hex::encode(Sha256::digest(&bytes))));
        Ok(anchored)
    }
}

fn pending_count(pending: Option<&serde_json::Map<String, Value>>, field: &str) -> u32 {
    pending
        .and_then(|pending| pending.get(field))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default()
}

fn unavailable_codegraph() -> CodeGraphIndexSnapshot {
    CodeGraphIndexSnapshot {
        extraction_status: "unavailable".to_owned(),
        ..CodeGraphIndexSnapshot::default()
    }
}

fn nul_paths(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect()
}

#[derive(Debug, Clone)]
pub struct FixedReviewSourceProbe {
    snapshot: ReviewSourceSnapshot,
    digests: BTreeMap<String, String>,
}

impl FixedReviewSourceProbe {
    #[must_use]
    pub fn new(snapshot: ReviewSourceSnapshot) -> Self {
        Self {
            snapshot,
            digests: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_digest(mut self, path: impl Into<String>, digest: impl Into<String>) -> Self {
        self.digests.insert(path.into(), digest.into());
        self
    }
}

impl ReviewSourceProbe for FixedReviewSourceProbe {
    fn capture(
        &self,
        scope_paths: &[String],
        at_ms: i64,
    ) -> Result<ReviewSourceSnapshot, SourceProbeError> {
        let mut snapshot = self.snapshot.clone();
        snapshot.id = format!("rsnap_{}", Uuid::now_v7().simple());
        snapshot.scope_paths = scope_paths.to_vec();
        snapshot.captured_at_ms = at_ms;
        Ok(snapshot)
    }

    fn anchor(&self, requested: &EvidenceAnchor) -> Result<EvidenceAnchor, SourceProbeError> {
        let mut anchor = requested.clone();
        anchor.content_digest = Some(
            self.digests
                .get(&requested.path)
                .cloned()
                .unwrap_or_else(|| "sha256:fixed".to_owned()),
        );
        Ok(anchor)
    }
}

use crate::{CodeGraphIndexSnapshot, EvidenceAnchor, ReviewArtifactSnapshot, ReviewSourceSnapshot};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use uuid::Uuid;
use zuno_paths::bounded;

const MAX_SCOPE_PATHS: usize = 32;
const MAX_PATH_CHARS: usize = 1_024;
const MAX_SOURCE_FILE_BYTES: u64 = 4 * 1_024 * 1_024;
const MAX_SOURCE_TOTAL_BYTES: u64 = 32 * 1_024 * 1_024;
pub const MAX_EVIDENCE_OBSERVATION_BYTES: u64 = 32 * 1_024 * 1_024;
const MAX_COMMAND_OUTPUT_BYTES: usize = 8 * 1_024 * 1_024;
const MAX_COMMAND_ERROR_CHARS: usize = 1_024;

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
    #[error("review source path `{0}` is a symbolic link; review evidence must name a plain file")]
    Symlink(String),
    #[error("review source path `{0}` is not a plain file")]
    NotFile(String),
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
    #[error("review evidence path `{path}` ends at line {end}, past its {lines} line(s)")]
    RangeEndOutsideFile {
        path: String,
        end: u32,
        lines: usize,
    },
    #[error("review source path `{path}` is {actual} bytes, exceeding the {max}-byte file limit")]
    FileTooLarge { path: String, actual: u64, max: u64 },
    #[error("review source capture reached {actual} bytes, exceeding the {max}-byte total limit")]
    CaptureTooLarge { actual: u64, max: u64 },
    #[error(
        "review evidence observation reached {actual} bytes, exceeding the {max}-byte total limit"
    )]
    EvidenceTooLarge { actual: u64, max: u64 },
}

pub trait ReviewSourceProbe: Send + Sync + 'static {
    fn capture(
        &self,
        scope_paths: &[String],
        artifact_path: Option<&str>,
        at_ms: i64,
    ) -> Result<ReviewSourceSnapshot, SourceProbeError>;

    fn anchor(&self, requested: &EvidenceAnchor) -> Result<EvidenceAnchor, SourceProbeError>;

    fn anchor_batch(
        &self,
        requested: &[EvidenceAnchor],
        max_total_bytes: u64,
    ) -> Result<Vec<EvidenceAnchor>, SourceProbeError>;
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
            command.args(scope_paths.iter().map(|path| format!(":(literal){path}")));
        }
        bounded::output_limited(&mut command, bounded::GIT_TIMEOUT, MAX_COMMAND_OUTPUT_BYTES)
            .map(|output| Output {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            })
            .map_err(|source| SourceProbeError::Command {
                command: format!("git {}", arguments.join(" ")),
                detail: bounded_detail(&source.to_string()),
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
            detail: bounded_detail(String::from_utf8_lossy(&output.stderr).trim()),
        })
    }

    fn codegraph(&self) -> CodeGraphIndexSnapshot {
        let mut command = Command::new("codegraph");
        command.arg("status").arg(&self.root).arg("--json");
        let output =
            bounded::output_limited(&mut command, bounded::GIT_TIMEOUT, MAX_COMMAND_OUTPUT_BYTES);
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
        if scope_paths.len() > MAX_SCOPE_PATHS {
            return Err(SourceProbeError::InvalidScope(format!(
                "received {} paths, exceeding the limit of {MAX_SCOPE_PATHS}",
                scope_paths.len()
            )));
        }
        for path in scope_paths {
            let path_ref = Path::new(path);
            if path.trim().is_empty()
                || path.contains('\0')
                || path.chars().count() > MAX_PATH_CHARS
                || path_ref.is_absolute()
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

    fn read_plain_file(
        root: &Path,
        relative: &Path,
        display: &str,
    ) -> Result<Vec<u8>, SourceProbeError> {
        let directory = Dir::open_ambient_dir(root, ambient_authority()).map_err(|source| {
            SourceProbeError::Read {
                path: display.to_owned(),
                source,
            }
        })?;
        let initial =
            directory
                .symlink_metadata(relative)
                .map_err(|source| SourceProbeError::Read {
                    path: display.to_owned(),
                    source,
                })?;
        if initial.file_type().is_symlink() {
            return Err(SourceProbeError::Symlink(display.to_owned()));
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file =
            directory
                .open_with(relative, &options)
                .map_err(|source| SourceProbeError::Read {
                    path: display.to_owned(),
                    source,
                })?;
        let file = file.into_std();
        let metadata = file.metadata().map_err(|source| SourceProbeError::Read {
            path: display.to_owned(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(SourceProbeError::NotFile(display.to_owned()));
        }
        if metadata.len() > MAX_SOURCE_FILE_BYTES {
            return Err(SourceProbeError::FileTooLarge {
                path: display.to_owned(),
                actual: metadata.len(),
                max: MAX_SOURCE_FILE_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(
            metadata.len().min(MAX_SOURCE_FILE_BYTES.saturating_add(1)) as usize,
        );
        file.take(MAX_SOURCE_FILE_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| SourceProbeError::Read {
                path: display.to_owned(),
                source,
            })?;
        if bytes.len() as u64 > MAX_SOURCE_FILE_BYTES {
            return Err(SourceProbeError::FileTooLarge {
                path: display.to_owned(),
                actual: bytes.len() as u64,
                max: MAX_SOURCE_FILE_BYTES,
            });
        }
        Ok(bytes)
    }

    fn artifact(
        root: &Path,
        requested: Option<&str>,
    ) -> Result<Option<ReviewArtifactSnapshot>, SourceProbeError> {
        let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        Self::validate_scope(&[requested.to_owned()])?;
        let relative = Path::new(requested);
        let bytes = Self::read_plain_file(root, relative, requested)?;
        Ok(Some(ReviewArtifactSnapshot {
            path: relative.to_string_lossy().replace('\\', "/"),
            content_digest: format!("sha256:{}", hex::encode(Sha256::digest(bytes))),
        }))
    }

    fn evidence_relative(requested: &EvidenceAnchor) -> Result<PathBuf, SourceProbeError> {
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
        Ok(relative.to_owned())
    }

    fn anchor_from_bytes(
        requested: &EvidenceAnchor,
        relative: &Path,
        bytes: &[u8],
    ) -> Result<EvidenceAnchor, SourceProbeError> {
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
            if let Some(end) = requested.end_line
                && usize::try_from(end).unwrap_or(usize::MAX) > lines
            {
                return Err(SourceProbeError::RangeEndOutsideFile {
                    path: requested.path.clone(),
                    end,
                    lines,
                });
            }
        }
        let mut anchored = requested.clone();
        anchored.path = relative.to_string_lossy().replace('\\', "/");
        anchored.content_digest = Some(format!("sha256:{}", hex::encode(Sha256::digest(bytes))));
        Ok(anchored)
    }
}

impl ReviewSourceProbe for RepositoryReviewSourceProbe {
    fn capture(
        &self,
        scope_paths: &[String],
        artifact_path: Option<&str>,
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
        let mut captured_bytes = 0_u64;
        for path in nul_paths(&untracked) {
            let bytes = Self::read_plain_file(&root, Path::new(&path), &path)?;
            captured_bytes = captured_bytes.saturating_add(bytes.len() as u64);
            if captured_bytes > MAX_SOURCE_TOTAL_BYTES {
                return Err(SourceProbeError::CaptureTooLarge {
                    actual: captured_bytes,
                    max: MAX_SOURCE_TOTAL_BYTES,
                });
            }
            digest.update(path.as_bytes());
            digest.update([0]);
            digest.update(bytes);
            digest.update([0]);
        }
        let artifact = Self::artifact(&root, artifact_path)?;
        Ok(ReviewSourceSnapshot {
            id: format!("rsnap_{}", Uuid::now_v7().simple()),
            repository_root: root.to_string_lossy().into_owned(),
            head_sha,
            branch,
            worktree_path: root.to_string_lossy().into_owned(),
            dirty: !diff.is_empty() || !untracked.is_empty(),
            worktree_digest: format!("sha256:{}", hex::encode(digest.finalize())),
            scope_paths: scope_paths.to_vec(),
            artifact,
            codegraph: self.codegraph(),
            captured_at_ms: at_ms,
        })
    }

    fn anchor(&self, requested: &EvidenceAnchor) -> Result<EvidenceAnchor, SourceProbeError> {
        self.anchor_batch(
            std::slice::from_ref(requested),
            MAX_EVIDENCE_OBSERVATION_BYTES,
        )?
        .pop()
        .ok_or_else(|| SourceProbeError::NotFile(requested.path.clone()))
    }

    fn anchor_batch(
        &self,
        requested: &[EvidenceAnchor],
        max_total_bytes: u64,
    ) -> Result<Vec<EvidenceAnchor>, SourceProbeError> {
        let root = self.canonical_root()?;
        let mut total = 0_u64;
        let mut files = BTreeMap::<String, Vec<u8>>::new();
        let mut anchored = Vec::with_capacity(requested.len());
        for requested in requested {
            let relative = Self::evidence_relative(requested)?;
            let key = relative.to_string_lossy().replace('\\', "/");
            if !files.contains_key(&key) {
                let bytes = Self::read_plain_file(&root, &relative, &requested.path)?;
                total = total.saturating_add(bytes.len() as u64);
                if total > max_total_bytes {
                    return Err(SourceProbeError::EvidenceTooLarge {
                        actual: total,
                        max: max_total_bytes,
                    });
                }
                files.insert(key.clone(), bytes);
            }
            let bytes = files
                .get(&key)
                .expect("evidence bytes were inserted before anchoring");
            anchored.push(Self::anchor_from_bytes(requested, &relative, bytes)?);
        }
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

fn bounded_detail(detail: &str) -> String {
    if detail.chars().count() <= MAX_COMMAND_ERROR_CHARS {
        return detail.to_owned();
    }
    let mut bounded = detail
        .chars()
        .take(MAX_COMMAND_ERROR_CHARS.saturating_sub(1))
        .collect::<String>();
    bounded.push('…');
    bounded
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
        _artifact_path: Option<&str>,
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

    fn anchor_batch(
        &self,
        requested: &[EvidenceAnchor],
        _max_total_bytes: u64,
    ) -> Result<Vec<EvidenceAnchor>, SourceProbeError> {
        requested.iter().map(|anchor| self.anchor(anchor)).collect()
    }
}

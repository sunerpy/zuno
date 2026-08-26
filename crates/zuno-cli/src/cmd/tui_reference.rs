//! Host-side project references for the TUI.
//!
//! The index is built once before raw mode and capped twice: at 20,000 visited
//! entries and 2,000 retained candidates. The keystroke path therefore performs no
//! filesystem work and ranks at most 2,000 in-memory candidates. Submission reads run
//! on a blocking worker and refuse any single file over 51,200 bytes or 2,000 lines.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use zuno_llm::event::RequestContentBlock;
use zuno_tui::views::autocomplete::{Candidate, CandidateKind, CompletionSource, Trigger};
use zuno_tui::views::session::PromptSubmission;

pub(super) const REFERENCE_CANDIDATE_LIMIT: usize = 2_000;
pub(super) const REFERENCE_SCAN_LIMIT: usize = 20_000;
pub(super) const REFERENCE_MAX_BYTES: usize = 50 * 1_024;
const REFERENCE_MAX_LINES: usize = 2_000;
const REFERENCE_MAX_PER_PROMPT: usize = 16;

pub(super) struct ProjectFiles {
    candidates: Vec<Candidate>,
}

impl ProjectFiles {
    pub(super) fn build(root: &Path) -> Result<Self, String> {
        Self::build_with_limits(root, REFERENCE_SCAN_LIMIT, REFERENCE_CANDIDATE_LIMIT)
    }

    fn build_with_limits(
        root: &Path,
        scan_limit: usize,
        candidate_limit: usize,
    ) -> Result<Self, String> {
        if !root.is_dir() {
            return Err(format!(
                "cannot index project references: `{}` is not a directory",
                root.display()
            ));
        }
        let filter = zuno_watch::FilterBuilder::new(root)
            .gitignore(true)
            .build()
            .map_err(|error| error.to_string())?;
        let mut candidates = Vec::new();
        let walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                entry.path() == root || !filter.is_ignored(entry.path(), entry.file_type().is_dir())
            });
        for (scanned, entry) in walker.enumerate() {
            if scanned >= scan_limit || candidates.len() >= candidate_limit {
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if entry.path() == root || filter.is_ignored(entry.path(), entry.file_type().is_dir()) {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(root) else {
                continue;
            };
            let display = slash(relative);
            if display.is_empty() {
                continue;
            }
            if entry.file_type().is_dir() {
                candidates.push(
                    Candidate::new(format!("{display}/"), CandidateKind::Directory)
                        .inserting(format!("@{display}/")),
                );
            } else if entry.file_type().is_file() {
                candidates.push(
                    Candidate::new(&display, CandidateKind::File).inserting(format!("@{display} ")),
                );
            }
        }
        candidates.sort_by(|left, right| left.display.cmp(&right.display));
        Ok(Self { candidates })
    }
}

impl CompletionSource for ProjectFiles {
    fn candidates(&self, trigger: Trigger, _query: &str) -> Vec<Candidate> {
        match trigger {
            Trigger::Command => Vec::new(),
            Trigger::Reference => self.candidates.clone(),
        }
    }
}

pub(super) async fn resolve_submission(
    root: &Path,
    submission: PromptSubmission,
) -> Result<PromptSubmission, String> {
    enum Delivery {
        Direct,
        Queue,
        Steer,
    }
    let (submission, delivery) = match submission {
        PromptSubmission::Queue(submission) => (*submission, Delivery::Queue),
        PromptSubmission::Steer(submission) => (*submission, Delivery::Steer),
        submission => (submission, Delivery::Direct),
    };
    let wrap = |submission| match delivery {
        Delivery::Direct => submission,
        Delivery::Queue => PromptSubmission::Queue(Box::new(submission)),
        Delivery::Steer => PromptSubmission::Steer(Box::new(submission)),
    };
    let (text, existing_content) = match submission {
        PromptSubmission::Text(text) => (text, None),
        PromptSubmission::Content { text, content } => (text, Some(content)),
        submission => return Ok(wrap(submission)),
    };
    let references = reference_tokens(&text)?;
    let resolved = if references.is_empty() {
        existing_content.map_or_else(
            || PromptSubmission::Text(text.clone()),
            |content| PromptSubmission::Content {
                text: text.clone(),
                content,
            },
        )
    } else {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            resolve_text_submission(&root, text, existing_content, references)
        })
        .await
        .map_err(|error| format!("file reference worker failed: {error}"))??
    };
    Ok(wrap(resolved))
}

fn reference_tokens(text: &str) -> Result<Vec<String>, String> {
    let mut seen = HashSet::new();
    let mut references = Vec::new();
    for token in text.split_whitespace() {
        let Some(path) = token.strip_prefix('@').filter(|path| !path.is_empty()) else {
            continue;
        };
        if seen.insert(path.to_owned()) {
            references.push(path.to_owned());
        }
    }
    if references.len() > REFERENCE_MAX_PER_PROMPT {
        return Err(format!(
            "file reference refused: a prompt may inject at most {REFERENCE_MAX_PER_PROMPT} files"
        ));
    }
    Ok(references)
}

fn resolve_text_submission(
    root: &Path,
    text: String,
    existing_content: Option<Vec<RequestContentBlock>>,
    references: Vec<String>,
) -> Result<PromptSubmission, String> {
    let canonical_root = root.canonicalize().map_err(|error| {
        format!(
            "cannot resolve file references under `{}`: {error}",
            root.display()
        )
    })?;
    let mut content =
        existing_content.unwrap_or_else(|| vec![RequestContentBlock::Text { text: text.clone() }]);
    for reference in references {
        let path = resolve_path(&canonical_root, &reference)?;
        if let Some(image) = zuno_tui::views::attachment::load_image_file(&path)
            .map_err(|error| format!("file reference `@{reference}` refused: {error}"))?
        {
            content.push(RequestContentBlock::Text {
                text: format!("Referenced image: {reference}"),
            });
            content.push(RequestContentBlock::Image {
                filename: Some(image.filename),
                media_type: image.media_type,
                data: image.data,
            });
            continue;
        }
        let bytes = read_bounded(&path, &reference)?;
        let body = String::from_utf8(bytes).map_err(|_| {
            format!(
                "file reference `@{reference}` is neither UTF-8 text nor a supported PNG, JPEG, GIF, or WebP image"
            )
        })?;
        let lines = body.lines().count();
        if lines > REFERENCE_MAX_LINES {
            return Err(format!(
                "file reference `@{reference}` refused: {lines} lines exceeds the {REFERENCE_MAX_LINES}-line limit"
            ));
        }
        content.push(RequestContentBlock::Text {
            text: format!(
                "--- BEGIN REFERENCED FILE: {reference} ---\n{body}\n--- END REFERENCED FILE: {reference} ---"
            ),
        });
    }
    Ok(PromptSubmission::Content { text, content })
}

fn resolve_path(root: &Path, reference: &str) -> Result<PathBuf, String> {
    let relative = Path::new(reference);
    if relative.is_absolute() {
        return Err(format!(
            "file reference `@{reference}` refused: only project-relative paths are allowed"
        ));
    }
    let joined = root.join(relative);
    let canonical = joined.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("file reference `@{reference}` not found")
        } else {
            format!("file reference `@{reference}` is unreadable: {error}")
        }
    })?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "file reference `@{reference}` refused: the path escapes the project"
        ));
    }
    if !canonical.is_file() {
        return Err(format!(
            "file reference `@{reference}` refused: the path is not a regular file"
        ));
    }
    Ok(canonical)
}

fn read_bounded(path: &Path, reference: &str) -> Result<Vec<u8>, String> {
    let file = File::open(path)
        .map_err(|error| format!("file reference `@{reference}` is unreadable: {error}"))?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(REFERENCE_MAX_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("file reference `@{reference}` is unreadable: {error}"))?;
    if bytes.len() > REFERENCE_MAX_BYTES {
        return Err(format!(
            "file reference `@{reference}` refused: the file exceeds the 51,200-byte limit"
        ));
    }
    Ok(bytes)
}

fn slash(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
#[path = "tui_reference_tests.rs"]
mod tests;

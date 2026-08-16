//! Persistence for output too large to hand back in full.
//!
//! # What this owns
//!
//! Writing the complete text somewhere durable, and reading it back. Nothing here
//! decides what the model sees instead; that is the output-policy layer's call
//! (todo 72). The division matters: the full text must survive regardless of which
//! policy is in force, so storage cannot be entangled with the policy that reads it.
//!
//! # Divergence from the oracle: the filename carries the session
//!
//! The TypeScript store's `bound()` accepts a `sessionID` **and** a `toolCallID`
//! (`packages/core/src/tool-output-store.ts:19-23`) and uses neither: the write path
//! takes only the content, and the name it produces is
//! `tool_${Identifier.ascending()}` (`:129-136`). The mapping back to a session
//! exists solely in the persisted tool part's `outputPaths` field
//! (`session/message-updater.ts:313`), so if that metadata is lost or a session is
//! deleted, the files on disk cannot be attributed to anything. The oracle's own
//! cleanup (`:176-190`) works around this by pruning purely on age.
//!
//! Todo 83's prune needs that attribution, so the session id is encoded in the
//! filename here. The `tool_` prefix is preserved deliberately: the oracle's cleanup
//! skips any entry that does not start with `tool_`, so files written by this binary
//! remain prunable by the TypeScript one sharing the same directory. The unique
//! component is a UUIDv7, which keeps the ascending-by-creation ordering that
//! `Identifier.ascending()` provides.

use crate::output::line_count;
use std::path::{Path, PathBuf};
use zuno_error::ToolError;
use zuno_paths::Layout;

/// The filename prefix the oracle's cleanup requires. `tool-output-store.ts:180`.
pub const FILE_PREFIX: &str = "tool_";

/// A persisted copy of a tool's full output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOutput {
    /// Where the full text lives.
    pub path: PathBuf,
    /// UTF-8 bytes written.
    pub bytes: usize,
    /// Lines written, counted as [`crate::output::line_count`] does.
    pub lines: usize,
}

/// The on-disk store for full tool output.
///
/// Writes are synchronous. A tool result is bounded by whatever produced it and the
/// write is a single `create_new` call, so the cost of a blocking write is far below
/// the cost of the thread hop needed to avoid it; a caller that disagrees can wrap
/// [`ToolOutputStore::persist`] in `spawn_blocking`.
#[derive(Debug, Clone)]
pub struct ToolOutputStore {
    root: PathBuf,
}

impl ToolOutputStore {
    /// A store rooted at an explicit directory. Created on first write.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// A store at `data()/tool-output`, the directory the oracle uses.
    #[must_use]
    pub fn in_layout(layout: &Layout) -> Self {
        Self::new(layout.tool_output())
    }

    /// The directory holding persisted output.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes `text` in full and returns where it went.
    ///
    /// `tool` names the tool for error reporting only. The file is created
    /// exclusively — matching the oracle's `flag: "wx"` — so a name collision fails
    /// loudly instead of overwriting another call's output.
    pub fn persist(
        &self,
        tool: &str,
        session_id: &str,
        text: &str,
    ) -> Result<StoredOutput, ToolError> {
        let failed = |error: std::io::Error| ToolError::Failed {
            tool: tool.to_owned(),
            source: Box::new(error),
        };

        std::fs::create_dir_all(&self.root).map_err(failed)?;
        let path = self.root.join(file_name(session_id));

        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(failed)?;
        file.write_all(text.as_bytes()).map_err(failed)?;
        file.flush().map_err(failed)?;

        Ok(StoredOutput {
            path,
            bytes: text.len(),
            lines: line_count(text),
        })
    }

    /// Reads persisted output back in full.
    pub fn read(&self, tool: &str, path: &Path) -> Result<String, ToolError> {
        std::fs::read_to_string(path).map_err(|error| ToolError::Failed {
            tool: tool.to_owned(),
            source: Box::new(error),
        })
    }

    /// Every persisted file in this store, in ascending creation order.
    ///
    /// Ascending because the unique component is a UUIDv7, whose hex form sorts by
    /// creation time. Entries that do not carry the prefix are ignored, matching the
    /// oracle's cleanup filter.
    pub fn entries(&self, tool: &str) -> Result<Vec<PathBuf>, ToolError> {
        let read = match std::fs::read_dir(&self.root) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(ToolError::Failed {
                    tool: tool.to_owned(),
                    source: Box::new(error),
                });
            }
        };

        let mut paths: Vec<PathBuf> = read
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(FILE_PREFIX))
            })
            .collect();
        paths.sort();
        Ok(paths)
    }
}

/// The name for a new file: `tool_<session>_<uuidv7>`.
///
/// The session component is sanitized to `[A-Za-z0-9_-]` so no identifier can escape
/// the store directory or collide with a path separator.
#[must_use]
fn file_name(session_id: &str) -> String {
    format!(
        "{FILE_PREFIX}{}_{}",
        sanitize(session_id),
        uuid::Uuid::now_v7().simple()
    )
}

/// The session a persisted file belongs to, or `None` when it cannot be attributed.
///
/// Splits from the right, so a session id containing `_` — which every `ses_…`
/// identifier does — is recovered whole. Returns `None` for a name the oracle wrote,
/// which is the honest answer: that name records no session.
#[must_use]
pub fn session_of(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let body = name.strip_prefix(FILE_PREFIX)?;
    let (session, unique) = body.rsplit_once('_')?;
    if session.is_empty() || unique.is_empty() {
        return None;
    }
    Some(session)
}

/// Replaces every character outside `[A-Za-z0-9_-]` with `-`.
#[must_use]
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_then_read_returns_the_full_text_unchanged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        let text = "line\n".repeat(5_000);

        let stored = store.persist("bash", "ses_abc", &text).expect("persist");
        let back = store.read("bash", &stored.path).expect("read");

        assert_eq!(back, text, "the full text must survive byte for byte");
        assert_eq!(stored.bytes, text.len());
        assert_eq!(stored.lines, 5_001);
    }

    #[test]
    fn persist_creates_the_directory_on_first_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("nested").join("tool-output");
        let store = ToolOutputStore::new(&root);

        let stored = store.persist("bash", "ses_abc", "hi").expect("persist");

        assert!(stored.path.starts_with(&root));
        assert!(root.is_dir());
    }

    #[test]
    fn names_keep_the_prefix_the_oracles_cleanup_requires() {
        let name = file_name("ses_abc");

        assert!(
            name.starts_with(FILE_PREFIX),
            "the TypeScript cleanup skips anything not prefixed {FILE_PREFIX}"
        );
    }

    #[test]
    fn a_persisted_file_is_attributable_to_its_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());

        let stored = store.persist("bash", "ses_abc123", "hi").expect("persist");

        assert_eq!(
            session_of(&stored.path),
            Some("ses_abc123"),
            "todo 83's prune cannot work without this"
        );
    }

    #[test]
    fn attribution_survives_a_session_id_containing_underscores() {
        assert_eq!(
            session_of(Path::new("/d/tool_ses_a_b_c_0199aabb")),
            Some("ses_a_b_c")
        );
    }

    #[test]
    fn an_oracle_written_name_reports_no_session_rather_than_guessing() {
        // `tool_${Identifier.ascending()}` has no session segment at all.
        assert_eq!(session_of(Path::new("/d/tool_01jqx8yz")), None);
        assert_eq!(session_of(Path::new("/d/snapshot_x_y")), None);
    }

    #[test]
    fn session_components_cannot_escape_the_store_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());

        let stored = store
            .persist("bash", "../../etc/ses_x", "hi")
            .expect("persist");

        assert_eq!(
            stored.path.parent(),
            Some(dir.path()),
            "a traversal in a session id must not move the file"
        );
        assert_eq!(session_of(&stored.path), Some("------etc-ses_x"));
    }

    #[test]
    fn two_writes_in_the_same_session_never_collide() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());

        let first = store.persist("bash", "ses_abc", "one").expect("first");
        let second = store.persist("bash", "ses_abc", "two").expect("second");

        assert_ne!(first.path, second.path);
        assert_eq!(store.read("bash", &first.path).expect("read"), "one");
        assert_eq!(store.read("bash", &second.path).expect("read"), "two");
    }

    #[test]
    fn entries_are_listed_in_creation_order_and_filtered_by_prefix() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        std::fs::write(dir.path().join("unrelated.txt"), "x").expect("write");

        let first = store.persist("bash", "ses_a", "one").expect("first");
        let second = store.persist("bash", "ses_a", "two").expect("second");

        assert_eq!(
            store.entries("bash").expect("entries"),
            vec![first.path, second.path]
        );
    }

    #[test]
    fn entries_on_a_store_that_was_never_written_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path().join("absent"));

        assert!(store.entries("bash").expect("entries").is_empty());
    }

    #[test]
    fn a_read_failure_names_the_tool_that_asked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());

        let error = store
            .read("bash", &dir.path().join("tool_missing_0"))
            .expect_err("absent file");

        assert_eq!(error.tool(), "bash");
        assert!(matches!(error, ToolError::Failed { .. }));
    }
}

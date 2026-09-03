//! Persistence for output too large to hand back in full.
//!
//! # What this owns
//!
//! Writing the bytes a tool produced somewhere durable, and reading them back one
//! bounded window at a time. Nothing here decides what the model sees instead; that is
//! the output-policy layer's call (todo 72). The division matters: the full output must
//! survive regardless of which policy is in force, so storage cannot be entangled with
//! the policy that reads it.
//!
//! Both halves are load-bearing. A store that only wrote would leave the model with an
//! artifact it can describe and cannot open — the shape that produced a truncated
//! `tail -80` of an authoritative test summary and a needless re-run — so the read path
//! is windowed, session-scoped, and addressed by the name this store minted.
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

use crate::output::line_count_of;
use std::path::{Path, PathBuf};
use zuno_error::ToolError;
use zuno_paths::{GeneratedDirectory, Layout};

/// The filename prefix the oracle's cleanup requires. `tool-output-store.ts:180`.
pub const FILE_PREFIX: &str = "tool_";

/// A persisted copy of a tool's full output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOutput {
    /// Where the full output lives.
    pub path: PathBuf,
    /// Bytes written, exactly as the tool produced them.
    pub bytes: usize,
    /// Lines written, counted as [`crate::output::line_count`] does.
    pub lines: usize,
}

/// One window of a persisted artifact, and where the next window starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWindow {
    /// The bytes read: at most the requested length, and never more than remains.
    pub bytes: Vec<u8>,
    /// Absolute offset just past [`Self::bytes`]. The cursor the next read passes back.
    pub cursor: u64,
    /// What the artifact holds in total, so a caller can tell whether more remains.
    pub total: u64,
}

/// The on-disk store for full tool output.
///
/// Writes are synchronous. A tool result is bounded by whatever produced it and the
/// write is a single `create_new` call, so the cost of a blocking write is far below
/// the cost of the thread hop needed to avoid it; a caller that disagrees can wrap
/// [`ToolOutputStore::persist`] in `spawn_blocking`.
#[derive(Debug, Clone)]
pub struct ToolOutputStore {
    root: Root,
}

/// Which kind of directory a store writes into.
///
/// The distinction is not cosmetic: a store inside a checkout is generated project
/// state that must never reach a commit, and the only reliable moment to say so is the
/// call that creates the directory. A store under the shared data layout is outside
/// every repository and has nothing to exclude itself from.
#[derive(Debug, Clone)]
enum Root {
    /// A directory named outright by the caller.
    Plain(PathBuf),
    /// `<worktree>/.zuno/tool-output/`, which excludes itself as it is created.
    Generated(GeneratedDirectory),
}

impl Root {
    fn path(&self) -> &Path {
        match self {
            Self::Plain(path) => path,
            Self::Generated(directory) => directory.path(),
        }
    }

    fn ensure(&self) -> Result<&Path, Box<dyn std::error::Error + Send + Sync + 'static>> {
        match self {
            Self::Plain(path) => {
                std::fs::create_dir_all(path)?;
                Ok(path)
            }
            Self::Generated(directory) => Ok(directory.ensure()?),
        }
    }
}

impl ToolOutputStore {
    /// A store rooted at an explicit directory. Created on first write.
    ///
    /// For a directory outside every checkout. Inside one, use
    /// [`ToolOutputStore::in_worktree`], which excludes the directory from git as it
    /// creates it.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Root::Plain(root.into()),
        }
    }

    /// A store at `data()/tool-output`, the directory the oracle uses.
    #[must_use]
    pub fn in_layout(layout: &Layout) -> Self {
        Self::new(layout.tool_output())
    }

    /// A store at `<worktree>/.zuno/tool-output/`, excluded from git on creation.
    ///
    /// `worktree` is the root of the checkout, which the caller resolves with
    /// [`zuno_paths::generated_root`]; joining the project directory onto a session's
    /// own working directory instead puts the store somewhere no exclude pattern
    /// covers.
    #[must_use]
    pub fn in_worktree(worktree: &Path) -> Self {
        Self {
            root: Root::Generated(GeneratedDirectory::in_worktree(
                worktree,
                &zuno_paths::generated::TOOL_OUTPUT,
            )),
        }
    }

    /// The directory holding persisted output.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Writes `text` in full and returns where it went.
    ///
    /// Text-shaped convenience over [`Self::persist_bytes`], for output that was
    /// produced as a string rather than decoded into one.
    pub fn persist(
        &self,
        tool: &str,
        session_id: &str,
        text: &str,
    ) -> Result<StoredOutput, ToolError> {
        self.persist_bytes(tool, session_id, text.as_bytes())
    }

    /// Writes `bytes` exactly as produced and returns where they went.
    ///
    /// `tool` names the tool for error reporting only. The file is created
    /// exclusively — matching the oracle's `flag: "wx"` — so a name collision fails
    /// loudly instead of overwriting another call's output.
    ///
    /// Bytes and not text, because a command's output is under no obligation to be
    /// UTF-8 and this copy is the one that outlives the call. A caller that decoded
    /// first would persist `U+FFFD` where the bytes were and the original would be
    /// gone: the retrieval path could then only ever return the damage. Decoding for
    /// display stays on the caller's side of this call.
    pub fn persist_bytes(
        &self,
        tool: &str,
        session_id: &str,
        bytes: &[u8],
    ) -> Result<StoredOutput, ToolError> {
        let failed = |error: std::io::Error| ToolError::Failed {
            tool: tool.to_owned(),
            source: Box::new(error),
        };

        let root = self.root.ensure().map_err(|source| ToolError::Failed {
            tool: tool.to_owned(),
            source,
        })?;
        let path = root.join(file_name(session_id));

        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(failed)?;
        file.write_all(bytes).map_err(failed)?;
        file.flush().map_err(failed)?;

        Ok(StoredOutput {
            path,
            bytes: bytes.len(),
            lines: line_count_of(bytes),
        })
    }

    /// Reads one window of an artifact this session produced.
    ///
    /// The window is at most `limit` bytes from `offset`, and [`StoredWindow::cursor`]
    /// says where the next one begins, so a caller pages by handing that cursor back.
    /// Windowed rather than whole-file because the reason output is in this store at all
    /// is that returning it in full was withheld: a retrieval that could only return
    /// everything would hand back exactly the payload the limits declined, and a caller
    /// with no window would go back to slicing the file with a shell command.
    ///
    /// A window that stops short of the end is trimmed back to a UTF-8 boundary, so
    /// paging a text artifact never splits a code point across two reads. A window that
    /// reaches the end is returned byte for byte: there is no following read to align
    /// with, and the artifact is not required to be text.
    ///
    /// `path` is resolved by filename against this store's own root, so no spelling of a
    /// path — including one a model repeated back with a traversal in it — reads a file
    /// this store did not write. `session_id` has to be the session the name records,
    /// which is the reason the name carries it.
    ///
    /// # Errors
    ///
    /// [`ToolError::InvalidArgs`] when `path` does not name an artifact belonging to
    /// `session_id`, which is a correctable call rather than a failure, and
    /// [`ToolError::Failed`] when the named artifact cannot be read.
    pub fn read_window(
        &self,
        tool: &str,
        session_id: &str,
        path: &Path,
        offset: u64,
        limit: usize,
    ) -> Result<StoredWindow, ToolError> {
        use std::io::{Read as _, Seek as _};

        let invalid = |message: String| ToolError::InvalidArgs {
            tool: tool.to_owned(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                message,
            )),
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| name.starts_with(FILE_PREFIX))
            .ok_or_else(|| {
                invalid(format!(
                    "`{}` does not name persisted tool output",
                    zuno_paths::wire_path(path)
                ))
            })?;
        if session_of(Path::new(name)) != Some(sanitize(session_id).as_str()) {
            return Err(invalid(format!(
                "persisted output `{name}` was not written by this session"
            )));
        }

        let failed = |error: std::io::Error| ToolError::Failed {
            tool: tool.to_owned(),
            source: Box::new(error),
        };
        let mut file = std::fs::File::open(self.root.path().join(name)).map_err(failed)?;
        let total = file.metadata().map_err(failed)?.len();
        let from = offset.min(total);
        let length = usize::try_from(total - from)
            .unwrap_or(usize::MAX)
            .min(limit);
        file.seek(std::io::SeekFrom::Start(from)).map_err(failed)?;
        let mut bytes = Vec::with_capacity(length);
        file.take(length as u64)
            .read_to_end(&mut bytes)
            .map_err(failed)?;
        let read = bytes.len() as u64;
        if from + read < total {
            trim_incomplete_tail(&mut bytes);
        }
        Ok(StoredWindow {
            cursor: from + bytes.len() as u64,
            bytes,
            total,
        })
    }

    /// Every persisted file in this store, in ascending creation order.
    ///
    /// Ascending because the unique component is a UUIDv7, whose hex form sorts by
    /// creation time. Entries that do not carry the prefix are ignored, matching the
    /// oracle's cleanup filter.
    pub fn entries(&self, tool: &str) -> Result<Vec<PathBuf>, ToolError> {
        let read = match std::fs::read_dir(self.root()) {
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

/// Drops an incomplete UTF-8 sequence from the end of one window.
///
/// Only a tail that is the *prefix* of a longer sequence is dropped. Bytes that are
/// invalid wherever they sit are left alone, because they are what the tool produced and
/// no later read can repair them. The window is never emptied: a limit smaller than one
/// code point still has to make progress, or a caller paging by the returned cursor
/// would ask for the same window forever.
fn trim_incomplete_tail(bytes: &mut Vec<u8>) {
    if let Err(error) = std::str::from_utf8(bytes)
        && error.error_len().is_none()
        && error.valid_up_to() > 0
    {
        bytes.truncate(error.valid_up_to());
    }
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

    /// Every window of one artifact, joined. Retrieval is windowed by design, so a test
    /// that wants the whole thing pages for it exactly as a caller does.
    fn read_all(store: &ToolOutputStore, session_id: &str, path: &Path, window: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut cursor = 0u64;
        loop {
            let read = store
                .read_window("shell", session_id, path, cursor, window)
                .expect("read window");
            bytes.extend_from_slice(&read.bytes);
            cursor = read.cursor;
            if cursor >= read.total {
                return bytes;
            }
            assert!(
                !read.bytes.is_empty(),
                "a window short of the end must advance"
            );
        }
    }

    #[test]
    fn persist_then_read_returns_the_full_text_unchanged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        let text = "line\n".repeat(5_000);

        let stored = store.persist("shell", "ses_abc", &text).expect("persist");
        let back = read_all(&store, "ses_abc", &stored.path, 4_096);

        assert_eq!(
            back,
            text.as_bytes(),
            "the full text must survive byte for byte"
        );
        assert_eq!(stored.bytes, text.len());
        assert_eq!(stored.lines, 5_001);
    }

    #[test]
    fn output_that_is_not_utf8_is_persisted_and_returned_byte_for_byte() {
        // The bytes a command produced, not a decoding of them: `0x80` alone is no valid
        // sequence, and a store that took `&str` would have kept `U+FFFD` instead.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        let raw = [b'o', b'k', 0x80, b'\n', 0xff];

        let stored = store
            .persist_bytes("shell", "ses_raw", &raw)
            .expect("persist");

        assert_eq!(stored.bytes, 5);
        assert_eq!(
            stored.lines, 2,
            "a newline counts wherever the bytes decode"
        );
        assert_eq!(read_all(&store, "ses_raw", &stored.path, 5), raw);
    }

    #[test]
    fn a_window_reports_the_cursor_the_next_window_starts_at() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        let stored = store
            .persist("shell", "ses_page", "0123456789")
            .expect("persist");

        let first = store
            .read_window("shell", "ses_page", &stored.path, 0, 4)
            .expect("first window");
        assert_eq!(first.bytes, b"0123");
        assert_eq!(first.cursor, 4);
        assert_eq!(first.total, 10);

        let second = store
            .read_window("shell", "ses_page", &stored.path, first.cursor, 4)
            .expect("second window");
        assert_eq!(second.bytes, b"4567");
        assert_eq!(second.cursor, 8);

        let last = store
            .read_window("shell", "ses_page", &stored.path, second.cursor, 4)
            .expect("last window");
        assert_eq!(last.bytes, b"89");
        assert_eq!(
            last.cursor, last.total,
            "reaching the end has to be observable without a further read"
        );

        let past = store
            .read_window("shell", "ses_page", &stored.path, 99, 4)
            .expect("a cursor past the end");
        assert!(past.bytes.is_empty());
        assert_eq!(
            past.cursor, 10,
            "the cursor is clamped, not the request rejected"
        );
    }

    #[test]
    fn paging_a_text_artifact_never_splits_a_code_point() {
        // Four three-byte code points read four bytes at a time: every naive window
        // would end mid-character and both sides of the split would decode as U+FFFD.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        let text = "中文测试";
        let stored = store.persist("shell", "ses_cjk", text).expect("persist");

        let mut cursor = 0u64;
        let mut decoded = String::new();
        loop {
            let window = store
                .read_window("shell", "ses_cjk", &stored.path, cursor, 4)
                .expect("window");
            decoded.push_str(
                std::str::from_utf8(&window.bytes).expect("each window decodes on its own"),
            );
            cursor = window.cursor;
            if cursor >= window.total {
                break;
            }
        }

        assert_eq!(decoded, text);
    }

    #[test]
    fn a_window_at_the_end_keeps_bytes_that_are_not_a_split_code_point() {
        // A trailing lead byte is what the tool produced. There is no next window to
        // align with, so trimming it would silently lose output instead of aligning it.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        let stored = store
            .persist_bytes("shell", "ses_tail", &[b'a', 0xe4])
            .expect("persist");

        let window = store
            .read_window("shell", "ses_tail", &stored.path, 0, 8)
            .expect("window");

        assert_eq!(window.bytes, [b'a', 0xe4]);
        assert_eq!(window.cursor, window.total);
    }

    #[test]
    fn an_artifact_another_session_wrote_is_not_readable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        let stored = store
            .persist("shell", "ses_owner", "secret")
            .expect("persist");

        let error = store
            .read_window("shell", "ses_other", &stored.path, 0, 8)
            .expect_err("attribution is enforced by the store, not by its callers");

        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    }

    #[test]
    fn a_path_outside_the_store_cannot_be_read_by_naming_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create");
        let store = ToolOutputStore::new(dir.path().join("store"));
        let stored = store.persist("shell", "ses_x", "mine").expect("persist");
        let name = stored.path.file_name().expect("name").to_owned();
        std::fs::write(elsewhere.join(&name), "theirs").expect("decoy");

        let read = store
            .read_window("shell", "ses_x", &elsewhere.join(&name), 0, 32)
            .expect("a name this store minted resolves against this store");
        assert_eq!(
            read.bytes, b"mine",
            "the directory in the request is ignored"
        );

        for outside in ["/etc/passwd", "../../etc/passwd"] {
            let error = store
                .read_window("shell", "ses_x", Path::new(outside), 0, 32)
                .expect_err("only names this store minted are addressable");
            assert!(matches!(error, ToolError::InvalidArgs { .. }), "{outside}");
        }
    }

    #[test]
    fn persist_creates_the_directory_on_first_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("nested").join("tool-output");
        let store = ToolOutputStore::new(&root);

        let stored = store.persist("shell", "ses_abc", "hi").expect("persist");

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

        let stored = store.persist("shell", "ses_abc123", "hi").expect("persist");

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
            .persist("shell", "../../etc/ses_x", "hi")
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

        let first = store.persist("shell", "ses_abc", "one").expect("first");
        let second = store.persist("shell", "ses_abc", "two").expect("second");

        assert_ne!(first.path, second.path);
        assert_eq!(read_all(&store, "ses_abc", &first.path, 32), b"one");
        assert_eq!(read_all(&store, "ses_abc", &second.path, 32), b"two");
    }

    #[test]
    fn entries_are_listed_in_creation_order_and_filtered_by_prefix() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());
        std::fs::write(dir.path().join("unrelated.txt"), "x").expect("write");

        let first = store.persist("shell", "ses_a", "one").expect("first");
        let second = store.persist("shell", "ses_a", "two").expect("second");

        assert_eq!(
            store.entries("shell").expect("entries"),
            vec![first.path, second.path]
        );
    }

    #[test]
    fn entries_on_a_store_that_was_never_written_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path().join("absent"));

        assert!(store.entries("shell").expect("entries").is_empty());
    }

    #[test]
    fn a_read_failure_names_the_tool_that_asked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path());

        let error = store
            .read_window("shell", "missing", &dir.path().join("tool_missing_0"), 0, 8)
            .expect_err("absent file");

        assert_eq!(error.tool(), "shell");
        assert!(matches!(error, ToolError::Failed { .. }));
    }

    #[test]
    fn a_store_in_a_worktree_lives_under_the_project_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::in_worktree(dir.path());

        assert_eq!(
            store.root(),
            dir.path()
                .join(zuno_paths::PROJECT_DIRECTORY)
                .join("tool-output"),
            "the store must sit where the exclude patterns are anchored"
        );
    }

    #[test]
    fn output_persisted_in_a_worktree_is_hidden_from_git_by_the_directory_itself() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::in_worktree(dir.path());

        let stored = store.persist("shell", "ses_abc", "hi").expect("persist");

        let marker = store.root().join(zuno_paths::SELF_EXCLUDE_FILE);
        assert!(
            marker.is_file(),
            "the write that created the directory must have excluded it"
        );
        assert!(
            std::fs::read_to_string(&marker)
                .expect("read marker")
                .lines()
                .any(|line| line == "*"),
            "the exclusion has to cover every entry, including the persisted output"
        );
        assert!(stored.path.starts_with(store.root()));
    }

    #[test]
    fn a_store_named_outright_is_not_excluded_from_anything() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ToolOutputStore::new(dir.path().join("data"));

        store.persist("shell", "ses_abc", "hi").expect("persist");

        assert!(
            !store.root().join(zuno_paths::SELF_EXCLUDE_FILE).exists(),
            "the shared data layout is outside every repository"
        );
    }
}

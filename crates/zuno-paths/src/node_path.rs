//! Node-compatible lexical path primitives with native platform semantics.
//!
//! # Why this module exists
//!
//! Every path in the TypeScript `opencode` is built with `path.join`,
//! `path.resolve` or `path.dirname`, and **Node normalizes**: `path.join` runs
//! its result through `normalize`, which collapses repeated separators and
//! resolves `.` and `..` textually. Rust's [`std::path::PathBuf::push`] does
//! none of that by itself.
//!
//! That difference is observable. Measured against the real 1.18.12 binary:
//!
//! | `XDG_DATA_HOME` | oracle `data` | `PathBuf::join` would give |
//! | --- | --- | --- |
//! | `/tmp/x//data` | `/tmp/x/data/opencode` | `/tmp/x//data/opencode` |
//! | `/tmp/x/data/` | `/tmp/x/data/opencode` | `/tmp/x/data/opencode` |
//! | `/tmp/x/../y` | `/tmp/y/opencode` | `/tmp/x/../y/opencode` |
//! | `x/y/..` | `x/opencode` | `x/y/../opencode` |
//! | `a/../../b` | `../b/opencode` | `a/../../b/opencode` |
//! | `..` | `../opencode` | `../opencode` |
//!
//! On Unix the implementation preserves the measured `path.posix` behavior.
//! On Windows it uses native drive, UNC, and separator semantics so an absolute
//! `ZUNO_DB`, Git path, or configuration path never becomes a relative filename
//! below the data directory.
//!
//! # Scope
//!
//! All operations are lexical and never touch the filesystem. Symlink-aware
//! canonicalization belongs at the boundary that actually discovers an existing
//! path, not in these helpers.

/// The POSIX separator. Node hard-codes `'/'` in `path.posix`.
#[cfg(not(windows))]
const SEP: char = '/';

/// Port of Node's internal `normalizeString(path, allowAboveRoot, '/', …)`.
///
/// Resolves `.` and `..` textually — it never touches the filesystem, so a
/// `..` is *not* symlink-aware. That is exactly what Node does, and reproducing
/// the quirk matters: a symlinked worktree would otherwise hash differently
/// here than in the oracle.
///
/// The returned string has neither a leading nor a trailing separator.
#[cfg(not(windows))]
fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split(SEP) {
        match segment {
            "" | "." => {}
            ".." => match out.last() {
                // `out` can only ever hold ".." when `allow_above_root` is set,
                // because the branch below is the sole place one is pushed.
                Some(&"..") => out.push(".."),
                Some(_) => {
                    out.pop();
                }
                None => {
                    if allow_above_root {
                        out.push("..");
                    }
                }
            },
            other => out.push(other),
        }
    }
    out.join("/")
}

/// Port of `path.posix.normalize`.
///
/// ```
/// use zuno_paths::node_path::normalize;
/// #[cfg(not(windows))]
/// {
///     assert_eq!(normalize("/tmp/x//data"), "/tmp/x/data");
///     assert_eq!(normalize("/tmp/x/../y"), "/tmp/y");
///     assert_eq!(normalize("a/../../b"), "../b");
///     assert_eq!(normalize(""), ".");
///     assert_eq!(normalize("/.."), "/");
/// }
/// ```
#[must_use]
#[cfg(not(windows))]
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let absolute = path.starts_with(SEP);
    let trailing = path.ends_with(SEP);
    let mut normalized = normalize_string(path, !absolute);
    if normalized.is_empty() {
        if absolute {
            return "/".to_owned();
        }
        return if trailing {
            "./".to_owned()
        } else {
            ".".to_owned()
        };
    }
    if trailing {
        normalized.push(SEP);
    }
    if absolute {
        let mut rooted = String::with_capacity(normalized.len() + 1);
        rooted.push(SEP);
        rooted.push_str(&normalized);
        return rooted;
    }
    normalized
}

#[cfg(windows)]
fn has_trailing_separator(path: &str) -> bool {
    matches!(path.chars().next_back(), Some('/' | '\\'))
}

/// Lexically normalize a native Windows path without requiring it to exist.
#[must_use]
#[cfg(windows)]
pub fn normalize(path: &str) -> String {
    use std::ffi::OsString;
    use std::path::{Component, Path, PathBuf};

    if path.is_empty() {
        return ".".to_owned();
    }

    let trailing = has_trailing_separator(path);
    let mut prefix: Option<OsString> = None;
    let mut rooted = false;
    let mut parts: Vec<OsString> = Vec::new();

    for component in Path::new(path).components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_owned()),
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts
                    .last()
                    .is_some_and(|part| part.to_string_lossy() != "..")
                {
                    parts.pop();
                } else if !rooted {
                    parts.push(OsString::from(".."));
                }
            }
            Component::Normal(value) => parts.push(value.to_owned()),
        }
    }

    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix);
    }
    if rooted {
        normalized.push(std::path::MAIN_SEPARATOR_STR);
    }
    for part in parts {
        normalized.push(part);
    }

    let mut text = normalized.to_string_lossy().into_owned();
    if text.is_empty() {
        return if rooted {
            std::path::MAIN_SEPARATOR_STR.to_owned()
        } else if trailing {
            format!(".{}", std::path::MAIN_SEPARATOR)
        } else {
            ".".to_owned()
        };
    }
    if trailing && !has_trailing_separator(&text) {
        text.push(std::path::MAIN_SEPARATOR);
    }
    text
}

/// Port of `path.posix.join(...segments)`.
///
/// Empty segments are skipped before joining, and the concatenation is then
/// normalized — both are Node behaviours the layout depends on.
///
/// ```
/// use zuno_paths::node_path::join_all;
/// #[cfg(not(windows))]
/// {
///     assert_eq!(join_all(["/tmp/x/data/", "opencode"]), "/tmp/x/data/opencode");
///     assert_eq!(join_all(["/", "opencode"]), "/opencode");
///     assert_eq!(join_all(["", ""]), ".");
/// }
/// ```
#[must_use]
pub fn join_all<I, S>(segments: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut joined: Option<String> = None;
    for segment in segments {
        let segment = segment.as_ref();
        if segment.is_empty() {
            continue;
        }
        match joined.as_mut() {
            None => joined = Some(segment.to_owned()),
            Some(accumulated) => {
                accumulated.push(std::path::MAIN_SEPARATOR);
                accumulated.push_str(segment);
            }
        }
    }
    joined.map_or_else(|| ".".to_owned(), |joined| normalize(&joined))
}

/// Two-argument [`join_all`], which is the shape almost every oracle call site
/// uses (`path.join(Global.Path.data, "auth.json")`).
#[must_use]
pub fn join(base: &str, segment: &str) -> String {
    join_all([base, segment])
}

/// Port of `path.posix.resolve(cwd, ...segments)`.
///
/// `cwd` is an explicit parameter rather than `std::env::current_dir()` so the
/// function stays pure and testable; Node reads `process.cwd()` only as the
/// final fallback, which is the same position `cwd` occupies here.
///
/// ```
/// use zuno_paths::node_path::resolve;
/// #[cfg(not(windows))]
/// {
///     assert_eq!(resolve("/repo", &["sub/.."]), "/repo");
///     assert_eq!(resolve("/repo", &["/abs/path"]), "/abs/path");
///     assert_eq!(resolve("/repo", &[]), "/repo");
/// }
/// ```
#[must_use]
#[cfg(not(windows))]
pub fn resolve(cwd: &str, segments: &[&str]) -> String {
    let mut resolved = String::new();
    let mut absolute = false;
    // Node walks the argument list from the end, then falls through to the
    // working directory, and stops as soon as a segment is absolute. `cwd` is
    // therefore the last candidate, not a special case.
    for segment in segments.iter().rev().copied().chain(std::iter::once(cwd)) {
        if absolute {
            break;
        }
        if segment.is_empty() {
            continue;
        }
        let mut next = String::with_capacity(segment.len() + 1 + resolved.len());
        next.push_str(segment);
        next.push(SEP);
        next.push_str(&resolved);
        resolved = next;
        absolute = segment.starts_with(SEP);
    }
    let normalized = normalize_string(&resolved, !absolute);
    if absolute {
        let mut rooted = String::with_capacity(normalized.len() + 1);
        rooted.push(SEP);
        rooted.push_str(&normalized);
        return rooted;
    }
    if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized
    }
}

#[must_use]
#[cfg(windows)]
pub fn resolve(cwd: &str, segments: &[&str]) -> String {
    use std::path::{Path, PathBuf};

    let mut resolved = PathBuf::from(cwd);
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        let segment = Path::new(segment);
        if segment.is_absolute() {
            resolved = segment.to_owned();
        } else {
            resolved.push(segment);
        }
    }
    normalize(&resolved.to_string_lossy())
}

/// Port of `path.posix.dirname`.
///
/// Used where the oracle does `path.dirname(dotgit)` to turn a discovered
/// `<dir>/.git` back into `<dir>`. Note the `'//'` special case, which Node
/// really does emit and which a naive "cut at the last slash" misses.
///
/// ```
/// use zuno_paths::node_path::dirname;
/// #[cfg(not(windows))]
/// {
///     assert_eq!(dirname("/repo/.git"), "/repo");
///     assert_eq!(dirname("/.git"), "/");
///     assert_eq!(dirname(".git"), ".");
///     assert_eq!(dirname("//x"), "//");
/// }
/// ```
#[must_use]
#[cfg(not(windows))]
pub fn dirname(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let bytes = path.as_bytes();
    let rooted = bytes[0] == b'/';
    let mut end: Option<usize> = None;
    let mut matched_slash = true;
    let mut index = bytes.len();
    while index > 1 {
        index -= 1;
        if bytes[index] == b'/' {
            if !matched_slash {
                end = Some(index);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None => {
            if rooted {
                "/".to_owned()
            } else {
                ".".to_owned()
            }
        }
        Some(1) if rooted => "//".to_owned(),
        Some(end) => path[..end].to_owned(),
    }
}

#[must_use]
#[cfg(windows)]
pub fn dirname(path: &str) -> String {
    use std::path::Path;

    if path.is_empty() {
        return ".".to_owned();
    }
    let normalized = normalize(path);
    let normalized_path = Path::new(&normalized);
    match normalized_path.parent() {
        None => {
            if normalized_path.has_root() {
                normalized
            } else {
                ".".to_owned()
            }
        }
        Some(parent) => {
            let text = parent.to_string_lossy();
            if text.is_empty() {
                ".".to_owned()
            } else {
                text.into_owned()
            }
        }
    }
}

/// Port of `path.posix.parse(p).root`: `"/"` for an absolute path, `""`
/// otherwise.
///
/// This is the value the oracle uses as the project directory when no Git
/// repository can be discovered
/// (`{ id: ID.global, directory: path.parse(input).root }`).
#[must_use]
#[cfg(not(windows))]
pub fn root(path: &str) -> String {
    if path.starts_with(SEP) { "/" } else { "" }.to_owned()
}

#[must_use]
#[cfg(windows)]
pub fn root(path: &str) -> String {
    use std::path::{Component, Path, PathBuf};

    let mut root = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => {
                root.push(std::path::MAIN_SEPARATOR_STR);
                return root.to_string_lossy().into_owned();
            }
            _ => return String::new(),
        }
    }
    String::new()
}

/// Port of `path.posix.isAbsolute`.
///
/// Deliberately not [`std::path::Path::is_absolute`]: that is `cfg`-dependent,
/// and this module models POSIX only.
#[must_use]
#[cfg(not(windows))]
pub fn is_absolute(path: &str) -> bool {
    path.starts_with(SEP)
}

#[must_use]
#[cfg(windows)]
pub fn is_absolute(path: &str) -> bool {
    std::path::Path::new(path).is_absolute()
}

/// Port of `FSUtil.windowsPath`, which is the identity function on every
/// non-Windows platform (`if (process.platform !== "win32") return p`).
///
/// Kept as a named no-op rather than being inlined away so that the Windows
/// branch has an obvious home when a later todo adds one, and so the call sites
/// read the same as the oracle's.
#[must_use]
#[cfg(not(windows))]
pub fn windows_path(path: &str) -> String {
    path.to_owned()
}

#[must_use]
#[cfg(windows)]
pub fn windows_path(path: &str) -> String {
    path.replace('/', std::path::MAIN_SEPARATOR_STR)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    /// The six cases in this table were read off the real 1.18.12 binary by
    /// running `XDG_DATA_HOME=<input> opencode debug paths` and taking the
    /// `data` row, so they are oracle output rather than a reading of Node's
    /// source. `/` and `/..` are absent because they make the oracle abort:
    /// its eager `mkdir` tries to create `/opencode` and dies with `EACCES`.
    #[test]
    fn join_matches_oracle_probes() {
        let cases = [
            ("/tmp/x/data", "/tmp/x/data/opencode"),
            ("/tmp/x//data", "/tmp/x/data/opencode"),
            ("/tmp/x/data/", "/tmp/x/data/opencode"),
            ("/tmp/x/../y", "/tmp/y/opencode"),
            ("..", "../opencode"),
            ("./x", "x/opencode"),
            ("a/../../b", "../b/opencode"),
            ("x/y/..", "x/opencode"),
            ("relx", "relx/opencode"),
            ("/", "/opencode"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                join(input, "opencode"),
                expected,
                "join({input:?}, \"opencode\")"
            );
        }
    }

    #[test]
    fn normalize_covers_node_edge_cases() {
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("."), ".");
        assert_eq!(normalize("./"), "./");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("/../.."), "/");
        assert_eq!(normalize("//a//b//"), "/a/b/");
        assert_eq!(normalize("a/b/../../.."), "..");
        assert_eq!(normalize("a/b/../../../.."), "../..");
    }

    #[test]
    fn join_skips_empty_segments_like_node() {
        assert_eq!(join_all(["", "opencode"]), "opencode");
        assert_eq!(join_all(["/data", "", "log"]), "/data/log");
        assert_eq!(join_all([""; 3]), ".");
        assert_eq!(join_all(Vec::<String>::new()), ".");
        assert_eq!(
            join_all(["/data", "snapshot", "global", "abc"]),
            "/data/snapshot/global/abc"
        );
    }

    #[test]
    fn resolve_stops_at_the_first_absolute_segment() {
        assert_eq!(resolve("/cwd", &["a", "/b", "c"]), "/b/c");
        assert_eq!(resolve("/cwd", &["a", "b"]), "/cwd/a/b");
        assert_eq!(resolve("/cwd", &[""]), "/cwd");
        assert_eq!(resolve("", &["a"]), "a");
        assert_eq!(resolve("", &[]), ".");
        assert_eq!(resolve("/cwd", &["../.."]), "/");
    }

    #[test]
    fn dirname_matches_node() {
        assert_eq!(dirname(""), ".");
        assert_eq!(dirname("/"), "/");
        assert_eq!(dirname("//"), "/");
        assert_eq!(dirname("///a"), "//");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("/a/"), "/");
        assert_eq!(dirname("a/b"), "a");
        assert_eq!(dirname("a"), ".");
        assert_eq!(dirname("/repo/sub/.git"), "/repo/sub");
    }

    #[test]
    fn root_and_is_absolute_agree() {
        assert_eq!(root("/a/b"), "/");
        assert_eq!(root("a/b"), "");
        assert_eq!(root(""), "");
        assert!(is_absolute("/a"));
        assert!(!is_absolute("a"));
        assert!(!is_absolute(""));
    }

    #[test]
    fn windows_path_is_identity_off_windows() {
        assert_eq!(windows_path("/mnt/c/repo"), "/mnt/c/repo");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn absolute_drive_paths_stay_absolute() {
        assert!(is_absolute(r"C:\data\zuno.db"));
        assert_eq!(root(r"C:\data\zuno.db"), r"C:\");
        assert_eq!(
            join(r"C:\Users\agent\AppData\Local", "zuno"),
            r"C:\Users\agent\AppData\Local\zuno"
        );
    }

    #[test]
    fn normalization_collapses_native_segments() {
        assert_eq!(normalize(r"C:\tmp\x\..\zuno"), r"C:\tmp\zuno");
        assert_eq!(
            resolve(r"C:\repo", &[r"sub\..", "config"]),
            r"C:\repo\config"
        );
        assert_eq!(dirname(r"C:\repo\.git"), r"C:\repo");
    }

    #[test]
    fn git_forward_slashes_become_native_separators() {
        assert_eq!(windows_path("C:/repo/.git"), r"C:\repo\.git");
    }
}

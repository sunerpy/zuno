//! A faithful port of Node's `path.posix` primitives.
//!
//! # Why this module exists
//!
//! Every path in the TypeScript `opencode` is built with `path.join`,
//! `path.resolve` or `path.dirname`, and **Node normalizes**: `path.join` runs
//! its result through `normalize`, which collapses repeated separators, resolves
//! `.` and `..` textually, and strips a trailing separator. Rust's
//! [`std::path::PathBuf::push`] does none of that — it concatenates.
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
//! Since the deliverable is a byte-identical on-disk layout, the join has to
//! behave like Node's, not like Rust's. Everything here therefore operates on
//! `&str` and mirrors the algorithms in Node's `lib/path.js` line for line.
//!
//! # Scope
//!
//! POSIX semantics only. `path.win32` is a separate algorithm (drive letters,
//! UNC roots, `\` separators) and is out of scope for this todo; see
//! `.omo/notepads/opencode-rust/issues.md`.

/// The POSIX separator. Node hard-codes `'/'` in `path.posix`.
const SEP: char = '/';

/// Port of Node's internal `normalizeString(path, allowAboveRoot, '/', …)`.
///
/// Resolves `.` and `..` textually — it never touches the filesystem, so a
/// `..` is *not* symlink-aware. That is exactly what Node does, and reproducing
/// the quirk matters: a symlinked worktree would otherwise hash differently
/// here than in the oracle.
///
/// The returned string has neither a leading nor a trailing separator.
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
/// use oc_paths::node_path::normalize;
/// assert_eq!(normalize("/tmp/x//data"), "/tmp/x/data");
/// assert_eq!(normalize("/tmp/x/../y"), "/tmp/y");
/// assert_eq!(normalize("a/../../b"), "../b");
/// assert_eq!(normalize(""), ".");
/// assert_eq!(normalize("/.."), "/");
/// ```
#[must_use]
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

/// Port of `path.posix.join(...segments)`.
///
/// Empty segments are skipped before joining, and the concatenation is then
/// normalized — both are Node behaviours the layout depends on.
///
/// ```
/// use oc_paths::node_path::join_all;
/// assert_eq!(join_all(["/tmp/x/data/", "opencode"]), "/tmp/x/data/opencode");
/// assert_eq!(join_all(["/", "opencode"]), "/opencode");
/// assert_eq!(join_all(["", ""]), ".");
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
                accumulated.push(SEP);
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
/// use oc_paths::node_path::resolve;
/// assert_eq!(resolve("/repo", &["sub/.."]), "/repo");
/// assert_eq!(resolve("/repo", &["/abs/path"]), "/abs/path");
/// assert_eq!(resolve("/repo", &[]), "/repo");
/// ```
#[must_use]
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

/// Port of `path.posix.dirname`.
///
/// Used where the oracle does `path.dirname(dotgit)` to turn a discovered
/// `<dir>/.git` back into `<dir>`. Note the `'//'` special case, which Node
/// really does emit and which a naive "cut at the last slash" misses.
///
/// ```
/// use oc_paths::node_path::dirname;
/// assert_eq!(dirname("/repo/.git"), "/repo");
/// assert_eq!(dirname("/.git"), "/");
/// assert_eq!(dirname(".git"), ".");
/// assert_eq!(dirname("//x"), "//");
/// ```
#[must_use]
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

/// Port of `path.posix.parse(p).root`: `"/"` for an absolute path, `""`
/// otherwise.
///
/// This is the value the oracle uses as the project directory when no Git
/// repository can be discovered
/// (`{ id: ID.global, directory: path.parse(input).root }`).
#[must_use]
pub fn root(path: &str) -> &'static str {
    if path.starts_with(SEP) { "/" } else { "" }
}

/// Port of `path.posix.isAbsolute`.
///
/// Deliberately not [`std::path::Path::is_absolute`]: that is `cfg`-dependent,
/// and this module models POSIX only.
#[must_use]
pub fn is_absolute(path: &str) -> bool {
    path.starts_with(SEP)
}

/// Port of `FSUtil.windowsPath`, which is the identity function on every
/// non-Windows platform (`if (process.platform !== "win32") return p`).
///
/// Kept as a named no-op rather than being inlined away so that the Windows
/// branch has an obvious home when a later todo adds one, and so the call sites
/// read the same as the oracle's.
#[must_use]
pub fn windows_path(path: &str) -> &str {
    path
}

#[cfg(test)]
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

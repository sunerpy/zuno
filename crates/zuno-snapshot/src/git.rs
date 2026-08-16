//! The `git` invocation layer.
//!
//! # Why `git` is a subprocess and not a library
//!
//! The snapshot store is not a normal repository and the oracle does not treat it
//! like one. It drives `git` with a specific set of `-c` overrides, a private
//! index seeded by copying another repository's index file, `--pathspec-from-file`
//! with NUL separation, `write-tree` against that private index, and
//! `checkout-index -a -f` to restore. Reproducing that byte-for-byte matters more
//! than elegance here: the Rust binary and the TypeScript binary must be able to
//! read each other's stores. See `decisions.md` for the full rationale.
//!
//! # Injection safety
//!
//! Nothing in this module ever builds a shell string. Arguments are pushed onto an
//! `OsString` vector and handed to [`std::process::Command::args`], which passes
//! them to `execvp` as a vector — a worktree path containing a space, a single
//! quote, a `$`, or a newline is one argument, not several. File *names* are never
//! passed as arguments at all: they travel on stdin as NUL-separated
//! `:(top,literal)` pathspecs, which is both injection-proof and immune to
//! pathspec magic in a filename.

use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use crate::error::{Result, SnapshotError};

/// `core` in `packages/opencode/src/snapshot/index.ts:25`.
pub(crate) const CORE: &[&str] = &["-c", "core.longpaths=true", "-c", "core.symlinks=true"];

/// `cfg` in `packages/opencode/src/snapshot/index.ts:26`.
pub(crate) const CFG: &[&str] = &[
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
];

/// `quote` in `packages/opencode/src/snapshot/index.ts:27`.
pub(crate) const QUOTE: &[&str] = &[
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.quotepath=false",
];

/// A finished `git` invocation. Mirrors the oracle's `GitResult`, which folds a
/// spawn failure into `{ exitCode: 1 }`; here a spawn failure is a typed error and
/// the caller decides whether to tolerate it.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
}

impl Output {
    /// True when `git` exited zero.
    pub(crate) fn ok(&self) -> bool {
        self.status.success()
    }

    /// The exit code, or `None` when the process died from a signal.
    pub(crate) fn code(&self) -> Option<i32> {
        self.status.code()
    }

    /// Standard output decoded as UTF-8.
    pub(crate) fn text(&self, args: &[String]) -> Result<String> {
        String::from_utf8(self.stdout.clone()).map_err(|source| SnapshotError::Encoding {
            args: args.to_vec(),
            source,
        })
    }
}

/// An argument vector under construction.
///
/// Arguments are kept as [`OsString`] so a non-UTF-8 worktree path survives
/// intact; a lossy `Vec<String>` copy is retained purely for error messages.
#[derive(Clone, Debug, Default)]
pub(crate) struct Argv {
    args: Vec<OsString>,
}

impl Argv {
    /// An empty vector.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Start from a slice of literal flags such as [`QUOTE`].
    pub(crate) fn flags(flags: &[&str]) -> Self {
        let mut argv = Self::new();
        argv.extend(flags);
        argv
    }

    /// Append one argument.
    pub(crate) fn push(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Append several arguments.
    pub(crate) fn extend<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.push(arg);
        }
        self
    }

    /// The lossy rendering used in error messages and logs.
    pub(crate) fn display(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The arguments as passed to `execvp`.
    pub(crate) fn as_slice(&self) -> &[OsString] {
        &self.args
    }
}

/// Run `git` with `argv` in `cwd`.
///
/// `env` entries are added to the inherited environment, matching the oracle's
/// `extendEnv: true`. `stdin` is written to the child and the pipe is then closed,
/// which is what `--pathspec-from-file=-` waits for.
pub(crate) fn run(
    argv: &Argv,
    cwd: &Path,
    env: &[(&OsStr, &OsStr)],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    let mut command = Command::new("git");
    command
        .args(argv.as_slice())
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    for (key, value) in env {
        command.env(key, value);
    }

    let spawn_error = |source: std::io::Error| SnapshotError::Spawn {
        args: argv.display(),
        cwd: cwd.to_path_buf(),
        source,
    };

    let mut child = command.spawn().map_err(spawn_error)?;
    if let Some(bytes) = stdin {
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| spawn_error(std::io::Error::other("git stdin pipe was not created")))?;
        pipe.write_all(bytes).map_err(spawn_error)?;
        // Dropping the handle closes the pipe; `--pathspec-from-file=-` blocks
        // until it sees EOF.
        drop(pipe);
    }
    let output = child.wait_with_output().map_err(spawn_error)?;

    Ok(Output {
        status: output.status,
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `encodeNulTerminatedPaths` — every path NUL-terminated, including the last.
pub(crate) fn nul_terminated(files: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for file in files {
        bytes.extend_from_slice(file.as_bytes());
        bytes.push(0);
    }
    bytes
}

/// `encodeTopLevelLiteralPathspecs` — `:(top,literal)` disables pathspec magic
/// and glob interpretation, so a file literally named `*` or `:(exclude)x` is
/// staged as itself.
pub(crate) fn top_level_literal_pathspecs(files: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for file in files {
        bytes.extend_from_slice(b":(top,literal)");
        bytes.extend_from_slice(file.as_bytes());
        bytes.push(0);
    }
    bytes
}

/// Split `-z` output on NUL, dropping empties — the oracle's
/// `.split("\0").filter(Boolean)`.
pub(crate) fn split_nul(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_flag_sets_match_index_ts_25_to_27() {
        assert_eq!(
            CORE,
            ["-c", "core.longpaths=true", "-c", "core.symlinks=true"]
        );
        assert_eq!(&CFG[0..2], ["-c", "core.autocrlf=false"]);
        assert_eq!(&CFG[2..], CORE);
        assert_eq!(&QUOTE[..CFG.len()], CFG);
        assert_eq!(&QUOTE[CFG.len()..], ["-c", "core.quotepath=false"]);
    }

    #[test]
    fn a_path_with_a_space_and_a_quote_stays_one_argument() {
        let mut argv = Argv::flags(QUOTE);
        argv.push("--git-dir")
            .push(Path::new("/data/it's a store/x"))
            .push("write-tree");
        let rendered = argv.display();
        assert_eq!(rendered.len(), QUOTE.len() + 3);
        assert_eq!(rendered[QUOTE.len() + 1], "/data/it's a store/x");
    }

    #[test]
    fn pathspecs_are_nul_separated_and_literal() {
        let files = vec![":(exclude)odd".to_owned(), "a b'c.txt".to_owned()];
        assert_eq!(
            top_level_literal_pathspecs(&files),
            b":(top,literal):(exclude)odd\0:(top,literal)a b'c.txt\0".to_vec()
        );
        assert_eq!(
            nul_terminated(&files),
            b":(exclude)odd\0a b'c.txt\0".to_vec()
        );
    }

    #[test]
    fn split_nul_drops_the_trailing_empty_field() {
        assert_eq!(split_nul("a\0b\0"), vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(split_nul(""), Vec::<String>::new());
    }

    #[test]
    fn run_captures_stdout_and_reports_the_exit_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut argv = Argv::new();
        argv.push("rev-parse").push("--git-dir");
        let output = run(&argv, dir.path(), &[], None).expect("spawn git");
        assert!(!output.ok(), "a temp dir is not a repository");
        assert!(output.stderr.contains("not a git repository"));
    }

    #[test]
    fn run_forwards_stdin_to_the_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut argv = Argv::new();
        argv.push("hash-object").push("--stdin");
        let output = run(&argv, dir.path(), &[], Some(b"hello\n")).expect("spawn git");
        assert!(output.ok(), "stderr: {}", output.stderr);
        assert_eq!(
            output.text(&argv.display()).expect("utf-8").trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
}

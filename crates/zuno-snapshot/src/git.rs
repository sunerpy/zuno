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
    ///
    /// This fails the whole call rather than substituting `U+FFFD`, which is the
    /// deliberate choice and worth stating: in a worktree holding one latin-1 name
    /// such as `caf\xe9.txt`, `Store::patch` and every capture report
    /// `SnapshotError::Encoding` and list nothing at all instead of naming a path
    /// that does not exist. Reporting nothing is recoverable; a wrong path is not.
    /// Carrying git's `-z` output as bytes and going lossy only at the display
    /// boundary would serve those worktrees, and is a change to every `-z` reader
    /// rather than to this decoder.
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
///
/// # Why the payload is written from another thread
///
/// stdout and stderr are pipes with a fixed kernel buffer, and not every caller
/// waits for EOF before it answers: `check-ignore --stdin -z` in [`crate::Store`]'s
/// `ignore` echoes a record for every matched path while it is still reading. Sending
/// the whole payload before touching stdout therefore stops dead once both buffers
/// fill — `git` blocked writing its answer, Zuno blocked writing the question — and
/// the per-git-directory lock is held across the call, so every later capture, undo
/// and patch for that worktree queues behind a call that can never return. The
/// payload goes to a scoped thread and this thread stays in `wait_with_output`, which
/// drains both output pipes at once, so neither side has to finish first.
///
/// The wait itself is deliberately not bounded. `checkout-index`, `write-tree` and
/// `apply` take as long as the worktree is large, and killing one part-way through
/// leaves a half-written index or working tree, which is a worse outcome than a slow
/// answer; what is removed here is the stall Zuno creates for itself.
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
    let payload = match stdin {
        Some(bytes) => {
            let pipe = child.stdin.take().ok_or_else(|| {
                spawn_error(std::io::Error::other("git stdin pipe was not created"))
            })?;
            Some((pipe, bytes))
        }
        None => None,
    };

    let (output, written) = std::thread::scope(|scope| {
        let writer = payload.map(|(mut pipe, bytes)| {
            // Dropping the handle as the thread ends closes the pipe;
            // `--pathspec-from-file=-` blocks until it sees EOF.
            scope.spawn(move || pipe.write_all(bytes))
        });
        let output = child.wait_with_output();
        let written = writer.map(std::thread::ScopedJoinHandle::join);
        (output, written)
    });

    // A short write is still the failure it was before the write moved off this
    // thread — a child that died early leaves a broken pipe here, and reporting the
    // exit status instead would let a truncated question look like a real answer.
    if let Some(written) = written {
        written
            .unwrap_or_else(|_| Err(std::io::Error::other("the git stdin writer panicked")))
            .map_err(spawn_error)?;
    }
    let output = output.map_err(spawn_error)?;

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
    use std::time::Duration;

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

    /// Four times the usual 64 KiB pipe capacity, so a payload of this size cannot
    /// cross the pipe unless somebody drains the answer at the same time.
    const OVERSIZED_PAYLOAD: usize = 256 * 1024;

    /// Probes long enough to reach [`OVERSIZED_PAYLOAD`] without an absurd count, all
    /// matching the fixture's `*.log` rule so every one of them is echoed back.
    fn oversized_probes() -> Vec<String> {
        let mut probes = Vec::new();
        let mut bytes = 0;
        while bytes < OVERSIZED_PAYLOAD {
            let probe = format!(
                "ignored/{:08}/some/rebuilt/artifact/chunk.log",
                probes.len()
            );
            bytes += probe.len() + 1;
            probes.push(probe);
        }
        probes
    }

    /// The `check-ignore` invocation from `Store::ignore`, which answers while it is
    /// still reading and so is the caller that a write-then-read `run` deadlocks.
    #[test]
    fn a_stdin_payload_larger_than_the_pipe_buffer_does_not_deadlock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut init = Argv::new();
        init.extend(["init", "-q", "."]);
        assert!(
            run(&init, dir.path(), &[], None).expect("spawn git").ok(),
            "the fixture repository is created"
        );
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").expect("write .gitignore");

        let probes = oversized_probes();
        let payload = nul_terminated(&probes);
        assert!(payload.len() > OVERSIZED_PAYLOAD, "{}", payload.len());

        // The call runs on a worker thread so the deadlock this test pins fails it
        // instead of wedging the whole test binary. On failure that thread and its
        // `git` child are deliberately left behind — the payload writer is inside
        // `run`, so the test cannot reach the child to kill it, and the harness reaps
        // both when it exits. A failing run can therefore leave one stuck
        // `check-ignore` per attempt.
        let worktree = dir.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut argv = Argv::flags(QUOTE);
            argv.push("--git-dir")
                .push(worktree.join(".git"))
                .push("--work-tree")
                .push(&worktree)
                .extend(["check-ignore", "--no-index", "--stdin", "-z"]);
            let outcome = run(&argv, &worktree, &[], Some(&payload))
                .map(|output| (output.ok(), output.stderr, output.stdout));
            let _ = sender.send(outcome);
        });

        let Ok(outcome) = receiver.recv_timeout(Duration::from_secs(60)) else {
            panic!("check-ignore never answered a payload larger than the pipe buffer");
        };
        let (ok, stderr, stdout) = outcome.expect("spawn git");
        assert!(ok, "every probe is ignored, so the exit is zero: {stderr}");
        assert_eq!(
            split_nul(&String::from_utf8(stdout).expect("utf-8")).len(),
            probes.len(),
            "every probe is echoed back"
        );
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

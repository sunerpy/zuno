//! Thin, cancellable adapter over the official `rg` executable.
//!
//! Zuno owns request validation and result shaping, but deliberately delegates
//! walking, ignore semantics, glob matching, regex execution, binary detection,
//! and filesystem traversal to ripgrep itself.

use crate::cancel::Cancellation;
use crate::error::SearchError;
use crate::types::{
    Entry, GlobRequest, GrepRequest, MAX_MATCH_TEXT, MAX_SUBMATCHES, Match, SearchResults,
    Submatch, normalize_relative, truncate_utf16,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

/// Oldest ripgrep major whose CLI and JSON stream Zuno accepts.
pub const MINIMUM_RIPGREP_MAJOR: u64 = 14;

/// Explicit exclusion appended after user globs so `.git` internals never surface.
const GIT_EXCLUDE_GLOB: &str = "!**/.git/**";
/// Maximum accepted size for one `rg --json` record.
const MAX_RECORD_BYTES: usize = 64 * 1024;
/// Cancellation polling interval while a child process is live.
const CANCEL_POLL: Duration = Duration::from_millis(10);

static SYSTEM_RIPGREP: OnceLock<Result<Discovery, DiscoveryFailure>> = OnceLock::new();

#[derive(Debug, Clone)]
struct Discovery {
    program: PathBuf,
    version: String,
}

#[derive(Debug, Clone)]
enum DiscoveryFailure {
    Missing,
    Probe { program: PathBuf, message: String },
    Version { program: PathBuf, found: String },
}

impl DiscoveryFailure {
    fn into_search_error(self) -> SearchError {
        let message = match self {
            Self::Missing => format!(
                "ripgrep (`rg`) is required for glob and grep; install ripgrep {MINIMUM_RIPGREP_MAJOR} or newer and ensure `rg` is on PATH"
            ),
            Self::Probe { program, message } => format!(
                "failed to inspect ripgrep at {}: {message}",
                program.display()
            ),
            Self::Version { program, found } => format!(
                "ripgrep at {} reports unsupported version `{found}`; Zuno requires major version {MINIMUM_RIPGREP_MAJOR} or newer",
                program.display()
            ),
        };
        SearchError::Unavailable { message }
    }
}

/// A search engine backed exclusively by one official `rg` executable.
#[derive(Debug, Clone)]
pub struct Ripgrep {
    program: PathBuf,
    version: Option<String>,
}

impl Ripgrep {
    /// Resolve and validate the process-wide `rg` on `PATH`.
    ///
    /// Discovery is cached because session remounts must not spawn `rg --version`
    /// repeatedly. Zuno's bootstrap process fixes the command environment before
    /// this value is first read.
    pub fn discover() -> Result<Self, SearchError> {
        let discovery = SYSTEM_RIPGREP
            .get_or_init(discover_system)
            .clone()
            .map_err(DiscoveryFailure::into_search_error)?;
        Ok(Self {
            program: discovery.program,
            version: Some(discovery.version),
        })
    }

    /// Use an explicit program without consulting `PATH`.
    ///
    /// Intended for tests, packaged companion binaries, and embedders that already
    /// validated the executable.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            version: None,
        }
    }

    /// The executable this engine invokes.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Version reported during system discovery, when available.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// List files matching one glob.
    pub fn glob(
        &self,
        request: &GlobRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Entry>, SearchError> {
        validate_root(&request.cwd)?;
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }

        let mut args = vec!["--no-config".to_owned(), "--files".to_owned()];
        if request.hidden {
            args.push("--hidden".to_owned());
        }
        if request.follow {
            args.push("--follow".to_owned());
        }
        args.push(format!("--glob={}", request.pattern));
        args.push(format!("--glob={GIT_EXCLUDE_GLOB}"));
        args.push(".".to_owned());

        let output = self.run(
            &request.cwd,
            &args,
            cancel,
            Some(Pattern::Glob(&request.pattern)),
        )?;
        let mut items = output
            .stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| Entry::file(normalize_relative(line)))
            .collect::<Vec<_>>();
        items.sort();
        let truncated = items.len() > request.limit;
        items.truncate(request.limit);
        Ok(SearchResults { items, truncated })
    }

    /// Search file contents for one regex.
    pub fn grep(
        &self,
        request: &GrepRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Match>, SearchError> {
        validate_root(&request.cwd)?;
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }

        let mut args = vec![
            "--no-config".to_owned(),
            "--json".to_owned(),
            "--hidden".to_owned(),
            "--no-messages".to_owned(),
        ];
        if let Some(include) = &request.include {
            args.push(format!("--glob={include}"));
        }
        args.push(format!("--glob={GIT_EXCLUDE_GLOB}"));
        args.push("--".to_owned());
        args.push(request.pattern.clone());
        args.push(request.file.clone().unwrap_or_else(|| ".".to_owned()));

        let output = self.run(
            &request.cwd,
            &args,
            cancel,
            Some(Pattern::Regex(&request.pattern)),
        )?;
        let mut items = Vec::new();
        for line in output.stdout.lines() {
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_RECORD_BYTES {
                return Err(SearchError::Ripgrep {
                    message: format!("JSON record exceeded {MAX_RECORD_BYTES} bytes"),
                });
            }
            if let Some(found) = parse_match(line)? {
                items.push(found);
            }
        }
        items.sort_by(|left, right| {
            left.entry
                .path
                .cmp(&right.entry.path)
                .then(left.line.cmp(&right.line))
                .then(left.offset.cmp(&right.offset))
        });
        let truncated = items.len() > request.limit;
        items.truncate(request.limit);
        Ok(SearchResults { items, truncated })
    }

    fn run(
        &self,
        cwd: &Path,
        args: &[String],
        cancel: &dyn Cancellation,
        pattern: Option<Pattern<'_>>,
    ) -> Result<RipgrepOutput, SearchError> {
        let mut child = Command::new(&self.program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| SearchError::Spawn {
                program: self.program.clone(),
                source,
            })?;
        let stdout = child
            .stdout
            .take()
            .expect("piped stdout exists immediately after spawn");
        let stderr = child
            .stderr
            .take()
            .expect("piped stderr exists immediately after spawn");
        let stdout_reader = read_stream(stdout);
        let stderr_reader = read_stream(stderr);

        let status = wait_for_exit(&mut child, cancel)?;
        let stdout = join_stream(stdout_reader, "stdout")?;
        let stderr = join_stream(stderr_reader, "stderr")?;
        classify_status(status, &stderr, pattern)?;
        Ok(RipgrepOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
        })
    }
}

fn discover_system() -> Result<Discovery, DiscoveryFailure> {
    let program = which::which("rg").map_err(|_| DiscoveryFailure::Missing)?;
    let output = Command::new(&program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| DiscoveryFailure::Probe {
            program: program.clone(),
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(DiscoveryFailure::Probe {
            program,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let first = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let version = first
        .strip_prefix("ripgrep ")
        .and_then(|value| value.split_whitespace().next())
        .unwrap_or_default();
    let major = version
        .split('.')
        .next()
        .and_then(|value| value.parse::<u64>().ok());
    if major.is_none_or(|major| major < MINIMUM_RIPGREP_MAJOR) {
        return Err(DiscoveryFailure::Version {
            program,
            found: first,
        });
    }
    Ok(Discovery {
        program,
        version: version.to_owned(),
    })
}

fn validate_root(root: &Path) -> Result<(), SearchError> {
    let metadata = std::fs::metadata(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SearchError::RootMissing {
                root: root.to_path_buf(),
            }
        } else {
            SearchError::Ripgrep {
                message: format!("could not inspect search root {}: {error}", root.display()),
            }
        }
    })?;
    if !metadata.is_dir() {
        return Err(SearchError::RootNotDirectory {
            root: root.to_path_buf(),
        });
    }
    Ok(())
}

fn read_stream<R>(mut reader: R) -> thread::JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join_stream(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    name: &str,
) -> Result<Vec<u8>, SearchError> {
    reader
        .join()
        .map_err(|_| SearchError::Ripgrep {
            message: format!("{name} reader panicked"),
        })?
        .map_err(|error| SearchError::Ripgrep {
            message: format!("reading {name} failed: {error}"),
        })
}

fn wait_for_exit(child: &mut Child, cancel: &dyn Cancellation) -> Result<ExitStatus, SearchError> {
    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SearchError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(CANCEL_POLL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SearchError::Ripgrep {
                    message: format!("waiting for process failed: {error}"),
                });
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Pattern<'a> {
    Glob(&'a str),
    Regex(&'a str),
}

fn classify_status(
    status: ExitStatus,
    stderr: &[u8],
    pattern: Option<Pattern<'_>>,
) -> Result<(), SearchError> {
    let code = status.code();
    let stderr = String::from_utf8_lossy(stderr).trim().to_owned();
    if code == Some(2) {
        match pattern {
            Some(Pattern::Regex(pattern)) if is_invalid_regex(&stderr) => {
                return Err(SearchError::InvalidPattern {
                    pattern: pattern.to_owned(),
                    message: stderr,
                });
            }
            Some(Pattern::Glob(pattern)) if is_invalid_glob(&stderr) => {
                return Err(SearchError::InvalidGlob {
                    pattern: pattern.to_owned(),
                    message: stderr,
                });
            }
            _ => {}
        }
    }
    // ripgrep: 0 = match, 1 = no match, 2 = partial filesystem/glob error.
    if !matches!(code, Some(0..=2)) {
        return Err(SearchError::Ripgrep {
            message: if stderr.is_empty() {
                format!("process exited with code {code:?}")
            } else {
                stderr
            },
        });
    }
    Ok(())
}

fn is_invalid_regex(stderr: &str) -> bool {
    stderr.contains("regex parse error") || stderr.contains("error parsing regex")
}

fn is_invalid_glob(stderr: &str) -> bool {
    stderr.contains("invalid glob")
        || stderr.contains("error parsing glob")
        || stderr.contains("unclosed character class")
}

struct RipgrepOutput {
    stdout: String,
}

/// Decode one `rg --json` line and ignore non-match records.
fn parse_match(line: &str) -> Result<Option<Match>, SearchError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| SearchError::Ripgrep {
            message: format!("invalid JSON output: {error}"),
        })?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("match") {
        return Ok(None);
    }
    let data = value.get("data").ok_or_else(|| SearchError::Ripgrep {
        message: "match record had no data".to_owned(),
    })?;
    let path = data
        .pointer("/path/text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "match record had no path".to_owned(),
        })?;
    let text = data
        .pointer("/lines/text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "match record had no line text".to_owned(),
        })?;
    let line_number = data
        .get("line_number")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "match record had no line number".to_owned(),
        })?;
    let offset = data
        .get("absolute_offset")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "match record had no absolute offset".to_owned(),
        })?;
    let submatches = data
        .get("submatches")
        .and_then(serde_json::Value::as_array)
        .map(|found| {
            found
                .iter()
                .take(MAX_SUBMATCHES)
                .filter_map(|entry| {
                    Some(Submatch {
                        text: entry
                            .pointer("/match/text")
                            .and_then(serde_json::Value::as_str)?
                            .to_owned(),
                        start: usize::try_from(
                            entry.get("start").and_then(serde_json::Value::as_u64)?,
                        )
                        .ok()?,
                        end: usize::try_from(entry.get("end").and_then(serde_json::Value::as_u64)?)
                            .ok()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(Match {
        entry: Entry::file(normalize_relative(path)),
        line: line_number,
        offset,
        text: truncate_utf16(text, MAX_MATCH_TEXT),
        submatches,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_discovery_reports_a_supported_version() {
        let engine = Ripgrep::discover().expect("test host provides supported rg");
        assert!(engine.program().is_file());
        let major = engine
            .version()
            .expect("discovery records version")
            .split('.')
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .expect("major version");
        assert!(major >= MINIMUM_RIPGREP_MAJOR);
    }

    #[test]
    fn a_non_match_record_is_skipped() {
        let begin = r#"{"type":"begin","data":{"path":{"text":"./a.ts"}}}"#;
        assert!(parse_match(begin).expect("begin parses").is_none());
    }

    #[test]
    fn a_match_record_is_reshaped() {
        let line = r#"{"type":"match","data":{"path":{"text":"./src/a.ts"},"lines":{"text":"alpha needle here\n"},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":6,"end":12}]}}"#;
        let found = parse_match(line)
            .expect("record parses")
            .expect("record matches");
        assert_eq!(found.entry.path, "src/a.ts");
        assert_eq!(found.line, 1);
        assert_eq!(found.submatches[0].start, 6);
    }

    #[test]
    fn malformed_json_is_typed() {
        assert!(matches!(
            parse_match("{not json").expect_err("malformed"),
            SearchError::Ripgrep { .. }
        ));
    }

    #[test]
    fn invalid_pattern_diagnostics_are_classified() {
        assert!(is_invalid_regex("regex parse error:\n  unclosed group"));
        assert!(is_invalid_regex("error parsing regex: bad"));
        assert!(is_invalid_glob("invalid glob pattern"));
        assert!(is_invalid_glob("error parsing glob: bad"));
    }
}

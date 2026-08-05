//! The fallback backend: a system `rg`, invoked once.
//!
//! # Why this exists at all
//!
//! The embedded engine is the default and needs no binary, which is the point of
//! this crate. This backend is here for one reason: when a divergence from the
//! oracle is suspected, the same requests can be answered by the very binary the
//! oracle would have used, which turns "our walker disagrees" into a question with
//! a mechanical answer. It is opt-in through [`Backend::from_env`].
//!
//! # Once, not per file
//!
//! `rg` is spawned a single time per request and given the whole directory. Spawning
//! per file — as `.omo/refs/claw-code` does in places — makes a search over fifty
//! thousand files fifty thousand process creations, which is the hang class this
//! port exists to remove, and it also loses the ignore-file semantics that only the
//! walker knows.
//!
//! # Ordering
//!
//! `rg` without `--sort` emits in a nondeterministic order, so results are sorted
//! here before truncation, giving both backends the same contract. That is the only
//! post-processing: the flags are the oracle's, verbatim.

use crate::cancel::Cancellation;
use crate::embedded::GIT_EXCLUDE_GLOB;
use crate::error::SearchError;
use crate::types::{
    Entry, GlobRequest, GrepRequest, MAX_MATCH_TEXT, MAX_SUBMATCHES, Match, SearchResults,
    Submatch, normalize_relative, truncate_utf16,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The size beyond which a `--json` record is refused.
///
/// `MAX_RECORD_BYTES` in `packages/core/src/ripgrep.ts:19`. Only this backend has
/// it, because only this backend has a JSON transport for a record to travel over;
/// the embedded engine reads the line straight out of the buffer and has no
/// equivalent failure.
const MAX_RECORD_BYTES: usize = 64 * 1024;

/// A search served by a `rg` binary on the host.
#[derive(Debug, Clone)]
pub struct RipgrepEngine {
    program: PathBuf,
}

impl RipgrepEngine {
    /// An engine backed by the binary at `program`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// The binary this engine will run.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Lists files matching a glob.
    ///
    /// # Errors
    ///
    /// [`SearchError::Spawn`] when `rg` cannot be started, [`SearchError::Ripgrep`]
    /// when it exits with an unexpected status, [`SearchError::Cancelled`] when the
    /// signal was already set.
    pub fn glob(
        &self,
        request: &GlobRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Entry>, SearchError> {
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

        let output = self.run(&request.cwd, &args, None)?;

        let mut items: Vec<Entry> = output
            .stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| Entry::file(normalize_relative(line)))
            .collect();
        items.sort();
        let truncated = items.len() > request.limit;
        items.truncate(request.limit);

        Ok(SearchResults { items, truncated })
    }

    /// Searches file contents for a regex.
    ///
    /// # Errors
    ///
    /// [`SearchError::InvalidPattern`] when `rg` reports a regex parse failure,
    /// [`SearchError::Spawn`], [`SearchError::Ripgrep`], [`SearchError::Cancelled`]
    /// as for [`RipgrepEngine::glob`].
    pub fn grep(
        &self,
        request: &GrepRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Match>, SearchError> {
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

        let output = self.run(&request.cwd, &args, Some(&request.pattern))?;

        let mut items = Vec::new();
        for line in output.stdout.lines() {
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_RECORD_BYTES {
                return Err(SearchError::Ripgrep {
                    message: format!("ripgrep JSON record exceeded {MAX_RECORD_BYTES} bytes"),
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
        pattern: Option<&str>,
    ) -> Result<RipgrepOutput, SearchError> {
        let output = Command::new(&self.program)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|source| SearchError::Spawn {
                program: self.program.clone(),
                source,
            })?;

        let code = output.status.code();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();

        // The oracle's status handling, `ripgrep.ts:135-141`: 0 is matches, 1 is no
        // matches, 2 is "some paths could not be searched" and is *not* fatal, and a
        // regex parse failure arrives as 2 plus a recognisable stderr.
        if code == Some(2) && pattern.is_some() && is_invalid_pattern(&stderr) {
            return Err(SearchError::InvalidPattern {
                pattern: pattern.unwrap_or_default().to_owned(),
                message: stderr,
            });
        }
        if !matches!(code, Some(0..=2)) {
            return Err(SearchError::Ripgrep {
                message: if stderr.is_empty() {
                    format!("ripgrep failed with code {code:?}")
                } else {
                    stderr
                },
            });
        }

        Ok(RipgrepOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        })
    }
}

struct RipgrepOutput {
    stdout: String,
}

fn is_invalid_pattern(stderr: &str) -> bool {
    stderr.contains("regex parse error") || stderr.contains("error parsing regex")
}

/// Decodes one `rg --json` line, yielding `None` for every record type other than
/// `match`.
///
/// The field names are `rg`'s, and the reshaping is the oracle's
/// (`ripgrep.ts:240-272`): the `./` prefix comes off the path, submatches are capped,
/// and the line text is capped.
fn parse_match(line: &str) -> Result<Option<Match>, SearchError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| SearchError::Ripgrep {
            message: format!("invalid ripgrep JSON output: {error}"),
        })?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("match") {
        return Ok(None);
    }
    let data = value.get("data").ok_or_else(|| SearchError::Ripgrep {
        message: "ripgrep match record had no data".to_owned(),
    })?;

    let path = data
        .pointer("/path/text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "ripgrep match record had no path".to_owned(),
        })?;
    let text = data
        .pointer("/lines/text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "ripgrep match record had no line text".to_owned(),
        })?;
    let line_number = data
        .get("line_number")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "ripgrep match record had no line number".to_owned(),
        })?;
    let offset = data
        .get("absolute_offset")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SearchError::Ripgrep {
            message: "ripgrep match record had no absolute offset".to_owned(),
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

/// Finds `rg` on `PATH`.
///
/// A hand-rolled scan rather than the `which` crate, which is not a workspace
/// dependency. On Windows the executable suffixes come from `PATHEXT` when it is
/// set, so a `rg.cmd` shim is found the same way the shell would find it.
#[must_use]
pub fn locate_ripgrep() -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .ok()
            .map(|value| {
                value
                    .split(';')
                    .filter(|extension| !extension.is_empty())
                    .map(|extension| format!("rg{}", extension.to_lowercase()))
                    .collect()
            })
            .unwrap_or_else(|| vec!["rg.exe".to_owned()])
    } else {
        vec!["rg".to_owned()]
    };

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| {
            names
                .iter()
                .map(move |name| dir.join(name))
                .collect::<Vec<_>>()
        })
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_match_record_is_skipped_rather_than_failing() {
        let begin = r#"{"type":"begin","data":{"path":{"text":"./a.ts"}}}"#;
        assert!(parse_match(begin).expect("a begin record parses").is_none());
    }

    #[test]
    fn a_match_record_is_reshaped_exactly_as_the_oracle_reshapes_it() {
        let line = r#"{"type":"match","data":{"path":{"text":"./src/a.ts"},"lines":{"text":"alpha needle here\n"},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":6,"end":12}]}}"#;

        let found = parse_match(line)
            .expect("a match record parses")
            .expect("a match record yields a match");

        assert_eq!(found.entry.path, "src/a.ts");
        assert_eq!(found.line, 1);
        assert_eq!(found.offset, 0);
        assert_eq!(found.text, "alpha needle here\n");
        assert_eq!(found.submatches.len(), 1);
        assert_eq!(found.submatches[0].start, 6);
    }

    #[test]
    fn malformed_json_is_a_typed_failure_and_not_a_panic() {
        let error = parse_match("{not json").expect_err("malformed input fails");
        assert!(matches!(error, SearchError::Ripgrep { .. }));
    }

    #[test]
    fn an_invalid_pattern_is_recognised_from_either_wording_ripgrep_uses() {
        assert!(is_invalid_pattern("regex parse error:\n  unclosed group"));
        assert!(is_invalid_pattern("error parsing regex: bad"));
        assert!(!is_invalid_pattern("No such file or directory"));
    }
}

//! Failures from loading, parsing, or validating configuration.

use crate::recovery::{Recoverable, Recovery};
use crate::source::BoxSource;
use std::path::PathBuf;

/// One validation problem found in a config file.
///
/// `key_path` is the structured location of the offending value —
/// `["provider", "anthropic", "options"]` — not a pre-rendered `provider.anthropic.options`
/// string, so a reporter can format it however it likes and a fixer can navigate
/// to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    pub key_path: Vec<String>,
    /// The validator's own description of the problem, carried verbatim for
    /// display. Payload, never a classification channel.
    pub detail: String,
}

impl ConfigIssue {
    pub fn new(
        key_path: impl IntoIterator<Item = impl Into<String>>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            key_path: key_path.into_iter().map(Into::into).collect(),
            detail: detail.into(),
        }
    }
}

/// A failure from loading, parsing, or validating configuration.
///
/// Every variant carries the path it failed on. A config error that cannot say
/// which of the layered config files was at fault is not actionable, and
/// recovering the path from a rendered message is the defect this crate exists to
/// prevent.
///
/// No variant is retryable: configuration does not fix itself. The variants exist
/// to tell a reporter *what to show* and a fixer *where to look*.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read. `std::io::ErrorKind` survives in the source, so
    /// a caller can still distinguish "absent" from "unreadable".
    #[error("config file {path} could not be read")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The file is not well-formed JSON. The concrete `serde_json::Error`
    /// preserves line and column for the reporter.
    #[error("config file {path} is not valid JSON")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// The file parsed but its contents are invalid. Each issue keeps its own key
    /// path so the reporter can point at every problem, not just the first.
    #[error("config file {path} failed validation ({} issue(s))", issues.len())]
    Invalid {
        path: PathBuf,
        issues: Vec<ConfigIssue>,
    },

    /// Frontmatter in a Markdown file (an agent or command definition) could not
    /// be parsed.
    #[error("frontmatter in {path} could not be parsed")]
    Frontmatter {
        path: PathBuf,
        #[source]
        source: BoxSource,
    },

    /// A directory next to a config file looks like a misspelling of one this
    /// project recognizes. `suggestion` is the name that was meant, so the
    /// reporter offers a fix rather than a complaint.
    #[error("directory {dir} under {path} looks like a typo for {suggestion}")]
    DirectoryTypo {
        path: PathBuf,
        dir: String,
        suggestion: String,
    },

    /// A remote config source needs credentials that are not available.
    #[error("remote config {url} on {remote} requires authentication")]
    RemoteAuth { url: String, remote: String },

    /// A config remains at the pre-Zuno path while the new config root is empty.
    #[error(
        "legacy config {old_path} was not loaded because Zuno uses {new_path}; copy it with: {copy_command}"
    )]
    LegacyConfig {
        old_path: PathBuf,
        new_path: PathBuf,
        copy_command: String,
    },
}

impl ConfigError {
    /// The action this failure calls for, which is always to surface it.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// The failure as a user should read it: the [`Display`](std::fmt::Display) line,
    /// then one line per issue naming its key path.
    ///
    /// `Display` deliberately stays a single summary line — several tests pin it, and
    /// a `{error}` interpolation should not spill across lines. But a summary alone
    /// makes `failed validation (1 issue(s))` an unactionable message, because the one
    /// thing the reader needs is *which* key. Anything reporting to a human should use
    /// this instead.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out = self.to_string();
        if let Self::Invalid { issues, .. } = self {
            for issue in issues {
                let key = if issue.key_path.is_empty() {
                    "<document>".to_owned()
                } else {
                    issue.key_path.join(".")
                };
                out.push_str(&format!("\n  {key}: {}", issue.detail));
            }
        }
        out
    }
}

impl Recoverable for ConfigError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Io { .. }
            | Self::Json { .. }
            | Self::Invalid { .. }
            | Self::Frontmatter { .. }
            | Self::DirectoryTypo { .. }
            | Self::RemoteAuth { .. }
            | Self::LegacyConfig { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_failure_keeps_the_error_kind_recoverable() {
        let e = ConfigError::Io {
            path: PathBuf::from("/etc/opencode/config.json"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        let ConfigError::Io { source, .. } = &e else {
            panic!("constructed an Io, matched something else");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            e.to_string(),
            "config file /etc/opencode/config.json could not be read"
        );
    }

    #[test]
    fn json_failure_keeps_line_and_column() {
        let source = serde_json::from_str::<serde_json::Value>("{\n  \"a\": ,\n}").unwrap_err();
        let line = source.line();
        let column = source.column();
        let e = ConfigError::Json {
            path: PathBuf::from("opencode.json"),
            source,
        };
        let ConfigError::Json { source, .. } = &e else {
            panic!("constructed a Json, matched something else");
        };
        assert_eq!((source.line(), source.column()), (line, column));
        assert_eq!(line, 2);
    }

    #[test]
    fn invalid_counts_its_issues_in_display_and_keeps_key_paths() {
        let e = ConfigError::Invalid {
            path: PathBuf::from("opencode.json"),
            issues: vec![
                ConfigIssue::new(["provider", "anthropic", "options"], "expected object"),
                ConfigIssue::new(["model"], "unknown model"),
            ],
        };
        assert_eq!(
            e.to_string(),
            "config file opencode.json failed validation (2 issue(s))"
        );
        let ConfigError::Invalid { issues, .. } = &e else {
            panic!("constructed an Invalid, matched something else");
        };
        assert_eq!(issues[0].key_path, vec!["provider", "anthropic", "options"]);
        assert_eq!(issues[1].detail, "unknown model");
    }

    #[test]
    fn report_names_every_offending_key_while_display_stays_one_line() {
        let e = ConfigError::Invalid {
            path: PathBuf::from("opencode.json"),
            issues: vec![
                ConfigIssue::new(["themes"], "unrecognized key"),
                ConfigIssue::new(["provider", "x", "options"], "expected object"),
                ConfigIssue::new(Vec::<String>::new(), "not an object"),
            ],
        };
        assert!(!e.to_string().contains('\n'));
        assert_eq!(
            e.report(),
            "config file opencode.json failed validation (3 issue(s))\n  \
             themes: unrecognized key\n  \
             provider.x.options: expected object\n  \
             <document>: not an object"
        );
    }

    #[test]
    fn report_of_a_non_validation_failure_is_just_its_display() {
        let e = ConfigError::Io {
            path: PathBuf::from("opencode.json"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        assert_eq!(e.report(), e.to_string());
    }

    #[test]
    fn directory_typo_carries_the_suggestion_a_fixer_needs() {
        let e = ConfigError::DirectoryTypo {
            path: PathBuf::from("/repo"),
            dir: ".opencode-agents".to_owned(),
            suggestion: ".opencode/agent".to_owned(),
        };
        assert_eq!(
            e.to_string(),
            "directory .opencode-agents under /repo looks like a typo for .opencode/agent"
        );
    }

    #[test]
    fn no_config_failure_is_retryable() {
        let errors = [
            ConfigError::Io {
                path: PathBuf::from("a"),
                source: std::io::Error::other("x"),
            },
            ConfigError::Json {
                path: PathBuf::from("a"),
                source: serde_json::from_str::<u8>("x").unwrap_err(),
            },
            ConfigError::Invalid {
                path: PathBuf::from("a"),
                issues: Vec::new(),
            },
            ConfigError::Frontmatter {
                path: PathBuf::from("a"),
                source: Box::new(std::io::Error::other("x")),
            },
            ConfigError::DirectoryTypo {
                path: PathBuf::from("a"),
                dir: "b".to_owned(),
                suggestion: "c".to_owned(),
            },
            ConfigError::RemoteAuth {
                url: "https://example.invalid/c.json".to_owned(),
                remote: "origin".to_owned(),
            },
            ConfigError::LegacyConfig {
                old_path: PathBuf::from("old/opencode.json"),
                new_path: PathBuf::from("new/opencode.json"),
                copy_command: "install old new".to_owned(),
            },
        ];
        for e in &errors {
            assert_eq!(e.recovery(), Recovery::Fail, "{e}");
            assert!(!Recoverable::is_retryable(e), "{e}");
            assert_eq!(Recoverable::retry_after(e), None, "{e}");
        }
    }
}

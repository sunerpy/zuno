//! Exercises the taxonomy across its public API, the way a downstream crate will.
//!
//! The unit tests inside `oc-error` can reach private detail; these cannot, which
//! is the point. Every assertion here is one a consumer crate could write, so a
//! change that breaks a consumer breaks this file.
//!
//! The theme running through it: **no assertion reads a rendered message to reach
//! a decision.** Where text is asserted, it is asserted as output for a human, and
//! the corresponding decision is taken from a field.

use oc_error::{
    ConfigError, ConfigIssue, DbError, Error, LspError, McpError, PluginError, ProviderError,
    Recoverable, Recovery, ToolError,
};
use std::error::Error as _;
use std::path::PathBuf;
use std::time::Duration;

/// The criterion this task exists to satisfy: the delay a provider sent survives
/// as data, readable without formatting anything.
#[test]
fn rate_limited_retry_after_is_the_duration_the_provider_sent() {
    assert_eq!(
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30))
        }
        .retry_after(),
        Some(Duration::from_secs(30))
    );
}

/// The defect being prevented, stated as a test. jcode reached a compaction
/// decision with `try_auto_compact_after_context_limit(&e.to_string())` and a retry
/// decision with `is_retryable_error(&message.to_lowercase())`. Both decisions are
/// available here from the variant alone, and the numbers a compactor needs survive
/// with them.
#[test]
fn recovery_decisions_need_no_access_to_rendered_text() {
    let cases: Vec<(ProviderError, Recovery)> = vec![
        (
            ProviderError::ContextLimit {
                limit_tokens: Some(200_000),
                used_tokens: Some(214_311),
            },
            Recovery::Compact,
        ),
        (
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            },
            Recovery::Retry {
                after: Some(Duration::from_secs(30)),
            },
        ),
        (
            ProviderError::Transient {
                status: Some(503),
                source: None,
            },
            Recovery::Retry { after: None },
        ),
        (
            ProviderError::Auth {
                provider: "anthropic".to_owned(),
                source: None,
            },
            Recovery::Reauthenticate,
        ),
        (
            ProviderError::Refused {
                provider: "anthropic".to_owned(),
                provider_text: None,
            },
            Recovery::Fail,
        ),
        (
            ProviderError::Fatal {
                status: Some(400),
                source: None,
            },
            Recovery::Fail,
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.recovery(), expected, "{error}");
        assert_eq!(error.is_retryable(), expected.is_retry(), "{error}");
        assert_eq!(error.retry_after(), expected.retry_after(), "{error}");
    }
}

/// A retry loop written against this taxonomy, with the branch structure the
/// compiler enforces. This is the shape every provider caller should have; it does
/// not mention a message anywhere.
#[test]
fn a_retry_loop_can_be_written_without_inspecting_a_message() {
    fn plan(error: &ProviderError) -> &'static str {
        match error.recovery() {
            Recovery::Retry { after: Some(_) } => "sleep the delay the provider named, then retry",
            Recovery::Retry { after: None } => "apply local backoff, then retry",
            Recovery::Compact => "compact the conversation, then send a smaller request",
            Recovery::Reauthenticate => "refresh credentials",
            Recovery::Fail => "surface to the user",
        }
    }

    assert_eq!(
        plan(&ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30))
        }),
        "sleep the delay the provider named, then retry"
    );
    assert_eq!(
        plan(&ProviderError::RateLimited { retry_after: None }),
        "apply local backoff, then retry"
    );
    assert_eq!(
        plan(&ProviderError::Transient {
            status: Some(529),
            source: None
        }),
        "apply local backoff, then retry"
    );
    assert_eq!(
        plan(&ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(214_311)
        }),
        "compact the conversation, then send a smaller request"
    );
    assert_eq!(
        plan(&ProviderError::Auth {
            provider: "openai".to_owned(),
            source: None
        }),
        "refresh credentials"
    );
    assert_eq!(
        plan(&ProviderError::Fatal {
            status: Some(404),
            source: None
        }),
        "surface to the user"
    );
}

/// Status codes jcode chased with `contains("503 service unavailable")` and friends,
/// classified from the number instead.
#[test]
fn status_codes_classify_without_a_substring_search() {
    let retryable = [408, 425, 429, 500, 502, 503, 504, 529];
    for status in retryable {
        let error = ProviderError::from_status("anthropic", status);
        assert!(error.is_retryable(), "status {status} should be retryable");
    }

    for status in [401, 403] {
        let error = ProviderError::from_status("anthropic", status);
        assert_eq!(error.recovery(), Recovery::Reauthenticate);
        assert!(!error.is_retryable(), "status {status} must not be retried");
    }

    for status in [400, 404, 422] {
        let error = ProviderError::from_status("anthropic", status);
        assert_eq!(error.recovery(), Recovery::Fail);
    }
}

/// claw-code's `RuntimeError { message: String }` could not say which tool failed.
/// Every variant here can, and the name is a field.
#[test]
fn tool_failures_identify_their_tool_and_their_next_step() {
    let denied = ToolError::Denied {
        tool: "write".to_owned(),
    };
    let invalid = ToolError::InvalidArgs {
        tool: "edit".to_owned(),
        source: Box::new(std::io::Error::other("missing field `oldString`")),
    };
    let timed_out = ToolError::Timeout {
        tool: "bash".to_owned(),
        elapsed: Duration::from_secs(120),
    };
    let missing = ToolError::NotFound {
        tool: "frobnicate".to_owned(),
    };
    let failed = ToolError::Failed {
        tool: "bash".to_owned(),
        source: Box::new(std::io::Error::other("exit status 1")),
    };

    assert_eq!(denied.tool(), "write");
    assert_eq!(invalid.tool(), "edit");
    assert_eq!(timed_out.tool(), "bash");
    assert_eq!(missing.tool(), "frobnicate");
    assert_eq!(failed.tool(), "bash");

    assert!(timed_out.is_retryable());
    assert!(!denied.is_retryable());
    assert!(!invalid.is_retryable());
    assert!(!missing.is_retryable());
    assert!(!failed.is_retryable());

    assert!(invalid.is_model_correctable());
    assert!(missing.is_model_correctable());
    assert!(!denied.is_model_correctable());
    assert!(!timed_out.is_model_correctable());
    assert!(!failed.is_model_correctable());
}

#[test]
fn config_failures_name_the_file_and_every_issue_in_it() {
    let error = ConfigError::Invalid {
        path: PathBuf::from("/repo/opencode.json"),
        issues: vec![
            ConfigIssue::new(["provider", "anthropic", "options"], "expected an object"),
            ConfigIssue::new(["model"], "unknown model `gpt-9`"),
        ],
    };

    let ConfigError::Invalid { path, issues } = &error else {
        panic!("constructed an Invalid, matched something else");
    };
    assert_eq!(path, &PathBuf::from("/repo/opencode.json"));
    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].key_path, ["provider", "anthropic", "options"]);
    assert_eq!(issues[1].detail, "unknown model `gpt-9`");
    assert_eq!(error.recovery(), Recovery::Fail);
}

#[test]
fn a_locked_database_is_retryable_and_nothing_else_in_that_domain_is() {
    let busy = DbError::Busy {
        retry_after: Some(Duration::from_millis(50)),
    };
    assert!(busy.is_retryable());
    assert_eq!(busy.retry_after(), Some(Duration::from_millis(50)));

    let missing = DbError::NotFound {
        table: "session".to_owned(),
        id: "ses_01".to_owned(),
    };
    assert!(!missing.is_retryable());
    assert_eq!(missing.retry_after(), None);
}

#[test]
fn transport_failures_are_separable_from_protocol_failures() {
    let unreachable = McpError::Connect {
        server: "playwright".to_owned(),
        source: Box::new(std::io::Error::other("connection refused")),
    };
    let undecodable = McpError::Protocol {
        server: "playwright".to_owned(),
        source: serde_json::from_str::<serde_json::Value>("{\"jsonrpc\":").unwrap_err(),
    };

    assert!(unreachable.is_retryable());
    assert!(!undecodable.is_retryable());
    assert_eq!(unreachable.server(), undecodable.server());
}

#[test]
fn a_missing_language_server_is_separable_from_one_that_will_not_start() {
    let missing = LspError::NotInstalled {
        server: "gopls".to_owned(),
        command: "gopls".to_owned(),
    };
    let unstartable = LspError::Spawn {
        server: "gopls".to_owned(),
        command: "gopls".to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
    };

    assert!(missing.is_missing_binary());
    assert!(!unstartable.is_missing_binary());
    assert!(!missing.is_retryable());
    assert!(!unstartable.is_retryable());
}

#[test]
fn plugin_failures_name_the_plugin_so_a_host_can_disable_it() {
    let error = PluginError::Hook {
        plugin: "oc-notify".to_owned(),
        hook: "tool.execute.after".to_owned(),
        source: Box::new(std::io::Error::other("TypeError: undefined")),
    };
    assert_eq!(error.plugin(), "oc-notify");
    assert!(!error.is_retryable());
    assert_eq!(
        error.source().map(ToString::to_string).as_deref(),
        Some("TypeError: undefined")
    );
}

#[test]
fn the_aggregate_error_routes_without_losing_the_domain_decision() {
    let aggregated = Error::from(ProviderError::RateLimited {
        retry_after: Some(Duration::from_secs(30)),
    });
    assert!(aggregated.is_retryable());
    assert_eq!(aggregated.retry_after(), Some(Duration::from_secs(30)));

    let Some(provider) = aggregated.as_provider() else {
        panic!("a provider failure must still be reachable as one");
    };
    assert_eq!(provider.retry_after(), Some(Duration::from_secs(30)));

    let from_db = Error::from(DbError::Busy { retry_after: None });
    assert!(from_db.is_retryable());
    assert!(from_db.as_provider().is_none());
}

/// A generic helper written once against [`Recoverable`], usable for every domain.
/// Without the trait each caller would re-derive the same decision, which is how
/// five copies of a retry predicate drift apart.
#[test]
fn one_generic_helper_serves_every_domain() {
    fn should_retry<E: Recoverable>(error: &E) -> bool {
        error.is_retryable()
    }

    assert!(should_retry(&ProviderError::Transient {
        status: Some(503),
        source: None
    }));
    assert!(should_retry(&DbError::Busy { retry_after: None }));
    assert!(should_retry(&ToolError::Timeout {
        tool: "bash".to_owned(),
        elapsed: Duration::from_secs(1)
    }));
    assert!(should_retry(&McpError::Timeout {
        server: "s".to_owned(),
        elapsed: Duration::from_secs(1)
    }));
    assert!(should_retry(&LspError::Exited {
        server: "s".to_owned(),
        code: Some(1)
    }));
    assert!(should_retry(&PluginError::Timeout {
        plugin: "p".to_owned(),
        hook: "h".to_owned(),
        elapsed: Duration::from_secs(1)
    }));
    assert!(!should_retry(&ConfigError::RemoteAuth {
        url: "https://example.invalid/c.json".to_owned(),
        remote: "origin".to_owned()
    }));
    assert!(!should_retry(&Error::from(ConfigError::RemoteAuth {
        url: "https://example.invalid/c.json".to_owned(),
        remote: "origin".to_owned()
    })));
}

/// Errors are still expected to render usefully. This asserts the human-facing
/// output and the full cause chain, which is what a `#[source]` chain is for — as
/// distinct from the routing decisions above, none of which touch this text.
#[test]
fn errors_render_a_full_cause_chain_for_humans() {
    let error = Error::from(ToolError::Failed {
        tool: "bash".to_owned(),
        source: Box::new(std::io::Error::other("exit status 1")),
    });

    assert_eq!(error.to_string(), "tool bash failed");

    let mut chain = vec![error.to_string()];
    let mut current: Option<&(dyn std::error::Error + 'static)> = error.source();
    while let Some(cause) = current {
        chain.push(cause.to_string());
        current = cause.source();
    }
    assert_eq!(chain, ["tool bash failed", "exit status 1"]);
}

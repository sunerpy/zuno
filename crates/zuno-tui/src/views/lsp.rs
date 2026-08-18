//! Language-server diagnostics as the transcript shows them.
//!
//! # Why a view layer needs this at all
//!
//! `zuno-lsp` has a working server pool, `touch_file`, and `diagnostics` — and until
//! this module nothing in the TUI referenced any of it. A user editing Rust in the
//! interactive surface had no way to learn that the edit did not compile until they
//! left and ran `cargo`, which is the whole reason the language-server integration
//! exists. The machinery was built, tested, and unreachable.
//!
//! # It is data, not a query
//!
//! Nothing here talks to a server. `zuno-tui` must not depend on `zuno-lsp` — a view
//! that spawned a subprocess to render a frame would block the one loop that consumes
//! terminal input — so the host runs the query beside the loop and pushes the result in
//! as a value, exactly the way a turn's events arrive.
//!
//! # An empty report is not the same as no report
//!
//! Three states, and they must not collapse into one:
//!
//! - No server claims the file. The user should be told that, because "no diagnostics"
//!   on a file nothing is checking is a false clean bill of health.
//! - A server checked it and found nothing. That is the good state and says so.
//! - A server found problems, listed worst-first.
//!
//! Reporting the first as the second is the "no results versus cannot see the data"
//! confusion that this codebase's other surfaces refuse, and it is more dangerous here
//! than elsewhere: it is a claim about correctness.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use crate::views::{ViewContext, display_width, padded, truncate};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

#[cfg(test)]
#[path = "lsp_tests.rs"]
mod tests;

/// How many paths may wait to be checked.
///
/// Distinct files, not batches: a session that writes more than this has produced more
/// diagnostics than a transcript can show. Bounded because the set outlives the turn
/// that filled it, so an unbounded one would be session-lifetime growth.
pub const PENDING_EDIT_LIMIT: usize = 4_096;

/// The files waiting to be checked, and the nudge that says so.
///
/// # Why the state is shared and only the wakeup is a message
///
/// The obvious shape — send the batch itself down a bounded channel — loses work. The
/// receiver serially awaits a language server's startup and its per-file diagnostics, so
/// several short turns finishing in a row fill the queue; a `try_send` that then reports
/// `Full` has dropped a *whole batch*, and the files unique to it are never re-checked.
/// The screen shows stale diagnostics, or none, with nothing saying so.
///
/// Refusing the newest is right for a prompt — the user can retype it — and wrong for
/// accumulated state, which nobody can retype. So the accumulated part is durable: paths
/// merge into one set that survives any number of missed wakeups, and the channel carries
/// `()` at capacity one purely as "there is something to do". Losing that signal costs
/// nothing, because the next one finds everything the lost one would have.
#[derive(Clone)]
pub struct PendingEdits {
    files: Arc<Mutex<BTreeSet<String>>>,
    /// Paths refused once [`PENDING_EDIT_LIMIT`] was reached.
    ///
    /// Counted rather than discarded silently: a truncated set is a gap in a claim about
    /// correctness, and the same reasoning that makes an unchecked file a reported state
    /// makes a dropped one worth naming.
    overflowed: Arc<Mutex<usize>>,
    wake: mpsc::Sender<()>,
}

impl PendingEdits {
    /// A handle that merges into a fresh set and nudges `wake`.
    #[must_use]
    pub fn new(wake: mpsc::Sender<()>) -> Self {
        Self {
            files: Arc::new(Mutex::new(BTreeSet::new())),
            overflowed: Arc::new(Mutex::new(0)),
            wake,
        }
    }

    /// Merge `paths` into the set and signal the checker.
    ///
    /// A poisoned lock is recovered rather than propagated: the guarded value is a set of
    /// strings, so a panic elsewhere cannot have left it in a state that makes an insert
    /// wrong, and a render loop that panicked over somebody else's panic would lose the
    /// whole session.
    pub fn merge(&self, paths: impl IntoIterator<Item = String>) {
        let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        let mut refused = 0_usize;
        for path in paths {
            if files.len() >= PENDING_EDIT_LIMIT && !files.contains(&path) {
                refused += 1;
                continue;
            }
            files.insert(path);
        }
        let empty = files.is_empty();
        drop(files);
        if refused > 0 {
            *self
                .overflowed
                .lock()
                .unwrap_or_else(PoisonError::into_inner) += refused;
        }
        if empty {
            return;
        }
        // `try_send` on a capacity-one channel: a full queue means a wakeup is already
        // pending, and that pending one will drain everything this call just merged.
        let _nudged = self.wake.try_send(());
    }

    /// A read handle over the same set, holding no wakeup sender.
    ///
    /// The consumer must not hold one. It would be its own producer, so the channel could
    /// never close, `recv` would never return `None`, and the checker task would outlive
    /// the screen that fed it — a task waiting forever on a sender only it still owns.
    /// Splitting the handle makes that unrepresentable rather than merely discouraged, and
    /// it states who may do what: the screen merges, the checker takes.
    #[must_use]
    pub fn reader(&self) -> PendingEditReader {
        PendingEditReader {
            files: Arc::clone(&self.files),
            overflowed: Arc::clone(&self.overflowed),
        }
    }
}

/// The consumer half of a [`PendingEdits`]: drains the set, cannot signal it.
#[derive(Clone)]
pub struct PendingEditReader {
    files: Arc<Mutex<BTreeSet<String>>>,
    overflowed: Arc<Mutex<usize>>,
}

impl PendingEditReader {
    /// Take every waiting path, and how many were refused for space.
    #[must_use]
    pub fn take(&self) -> (Vec<String>, usize) {
        let taken = std::mem::take(&mut *self.files.lock().unwrap_or_else(PoisonError::into_inner));
        let overflowed = std::mem::take(
            &mut *self
                .overflowed
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        (taken.into_iter().collect(), overflowed)
    }
}

/// How serious one diagnostic is.
///
/// The LSP severity numbers, named. `1` is an error and `2` a warning
/// (`zuno_lsp::client::Diagnostic::severity`); a server that omits the field is
/// treated as an error, because a message with no stated severity that turns out to be
/// a compile failure is the expensive direction to be wrong in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Severity `1`, or absent.
    Error,
    /// Severity `2`.
    Warning,
    /// Severity `3`.
    Information,
    /// Severity `4`.
    Hint,
}

impl Severity {
    /// The severity an LSP number names.
    #[must_use]
    pub const fn from_lsp(severity: Option<u32>) -> Self {
        match severity {
            Some(2) => Self::Warning,
            Some(3) => Self::Information,
            Some(4) => Self::Hint,
            _ => Self::Error,
        }
    }

    /// The word the transcript uses.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }

    /// The gutter glyph.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Error => "✗",
            Self::Warning => "△",
            Self::Information | Self::Hint => "·",
        }
    }
}

/// One diagnostic, flattened for display.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Diagnostic {
    /// How serious it is.
    pub severity: Severity,
    /// One-based line, as a human counts.
    ///
    /// One-based because LSP ranges are zero-based and every editor a user will paste
    /// this into is not; a report whose line numbers are off by one is worse than none.
    pub line: u32,
    /// One-based column.
    pub column: u32,
    /// The producing server or linter.
    pub source: Option<String>,
    /// The message, already a single line.
    pub message: String,
}

/// What a language server said about one file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Report {
    /// The file, as the user would name it.
    pub path: String,
    /// The server that answered, when one did.
    pub server: Option<String>,
    /// What it found, worst first.
    pub diagnostics: Vec<Diagnostic>,
}

impl Report {
    /// A report for a file no server claims.
    #[must_use]
    pub fn unchecked(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            server: None,
            diagnostics: Vec::new(),
        }
    }

    /// A report from `server`, sorted worst-first.
    #[must_use]
    pub fn checked(
        path: impl Into<String>,
        server: impl Into<String>,
        mut diagnostics: Vec<Diagnostic>,
    ) -> Self {
        // Worst first, then by position, so the row a user reads first is the one that
        // stops the build. Stable within a severity so two runs render identically.
        diagnostics.sort_by(|left, right| {
            left.severity
                .cmp(&right.severity)
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.column.cmp(&right.column))
        });
        Self {
            path: path.into(),
            server: Some(server.into()),
            diagnostics,
        }
    }

    /// Whether any server checked this file.
    #[must_use]
    pub const fn is_checked(&self) -> bool {
        self.server.is_some()
    }

    /// How many diagnostics of `severity` there are.
    #[must_use]
    pub fn count(&self, severity: Severity) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == severity)
            .count()
    }

    /// The one-line summary, which is what the status strip carries.
    #[must_use]
    pub fn summary(&self) -> String {
        let Some(server) = &self.server else {
            return format!("{}: no language server claims this file", self.path);
        };
        if self.diagnostics.is_empty() {
            return format!("{}: no problems ({server})", self.path);
        }
        let errors = self.count(Severity::Error);
        let warnings = self.count(Severity::Warning);
        let others = self.diagnostics.len() - errors - warnings;
        let mut parts = Vec::new();
        if errors > 0 {
            parts.push(format!("{errors} error{}", plural(errors)));
        }
        if warnings > 0 {
            parts.push(format!("{warnings} warning{}", plural(warnings)));
        }
        if others > 0 {
            parts.push(format!("{others} other{}", plural(others)));
        }
        format!("{}: {} ({server})", self.path, parts.join(", "))
    }

    /// The rendered rows, at most `limit` diagnostics.
    ///
    /// Capped, with the remainder counted, for the reason a tool result is: the
    /// transcript measures every row it produces in order to scroll, and a file with two
    /// thousand diagnostics would make that arithmetic walk two thousand rows per frame.
    #[must_use]
    pub fn lines(&self, width: u16, limit: usize, context: &ViewContext) -> Vec<Line<'static>> {
        let mut lines = vec![padded(
            &format!("  ⌁ {}", self.summary()),
            width,
            if self.count(Severity::Error) > 0 {
                context.error()
            } else if self.count(Severity::Warning) > 0 {
                context.warning()
            } else if self.is_checked() {
                context.success()
            } else {
                context.muted()
            },
        )];
        for diagnostic in self.diagnostics.iter().take(limit) {
            let location = format!("{}:{}", diagnostic.line, diagnostic.column);
            let source = diagnostic
                .source
                .as_deref()
                .map_or_else(String::new, |source| format!(" [{source}]"));
            let head = format!(
                "      {} {location:<9} {}{source}  ",
                diagnostic.severity.glyph(),
                diagnostic.severity.label()
            );
            let room = usize::from(width).saturating_sub(display_width(&head));
            let mut spans = vec![Span::styled(
                head,
                match diagnostic.severity {
                    Severity::Error => context.error(),
                    Severity::Warning => context.warning(),
                    Severity::Information | Severity::Hint => context.muted(),
                },
            )];
            let message = truncate(&diagnostic.message, room);
            let used = display_width(&message);
            spans.push(Span::styled(message, context.text()));
            if used < room {
                spans.push(Span::styled(" ".repeat(room - used), context.surface()));
            }
            lines.push(Line::from(spans));
        }
        let hidden = self.diagnostics.len().saturating_sub(limit);
        if hidden > 0 {
            lines.push(padded(
                &format!("      … {hidden} more"),
                width,
                context.muted(),
            ));
        }
        lines
    }
}

/// `s` when `count` is not one.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

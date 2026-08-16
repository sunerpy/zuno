//! Running language-server diagnostics beside the render loop.
//!
//! # Why this is a task and not a call
//!
//! Asking a language server for diagnostics means spawning it, waiting for it to index,
//! and reading a notification — hundreds of milliseconds on a warm cache and seconds on
//! a cold one. The TUI's event loop is the only consumer of terminal input, engine
//! events, and the terminal-lease wake, so a query awaited anywhere it can reach would
//! stall all three. It therefore runs in its own task and delivers a
//! [`zuno_tui::views::lsp::Report`] the same way a turn delivers events: as a value on a
//! bounded channel.
//!
//! # Only files the turn actually wrote
//!
//! Diagnostics for the whole project would be a second build, and most of it would be
//! about code the user did not touch. The set of paths comes from
//! [`zuno_tui::views::session::SessionScreen`], which already observes every
//! `ToolDispatchCompleted` in order to render it — so the edit set is derived from what
//! the user was shown rather than from a second, separately-wired listener that could
//! disagree with it.
//!
//! # A file no server claims is reported, not skipped
//!
//! [`zuno_tui::views::lsp::Report::unchecked`] exists for that case and the summary says
//! so. Silence on an unchecked file reads as a clean bill of health, and here that is a
//! false claim about correctness rather than merely an empty list.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use zuno_lsp::manager::Manager;
use zuno_tui::views::lsp::{Diagnostic, Report, Severity};

/// How many reports may be queued.
///
/// One per file in a turn's edit set, and a turn that edits more than this has already
/// given the user more than they can read; a full channel costs a report, never a stall.
pub(super) const REPORT_CHANNEL_CAPACITY: usize = 16;

/// Ask `manager` about each of `paths` and send one report per file.
pub(super) async fn report(
    manager: Arc<Manager>,
    workspace: PathBuf,
    paths: Vec<PathBuf>,
    reports: mpsc::Sender<Report>,
) {
    for path in paths {
        let display = path
            .strip_prefix(&workspace)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if !manager.has_server(&path) {
            let _sent = reports.try_send(Report::unchecked(display));
            continue;
        }
        // `touch_file` before `diagnostics`: a server that has never seen the file has
        // nothing to say about it, and the empty answer would be indistinguishable from
        // a clean one.
        if let Err(error) = manager.touch_file(&path).await {
            tracing::debug!(%error, path = %path.display(), "lsp could not open the file");
            let _sent = reports.try_send(Report::unchecked(display));
            continue;
        }
        let server = manager
            .status()
            .await
            .into_iter()
            .find(|status| path.starts_with(&status.root))
            .map_or_else(|| String::from("lsp"), |status| status.id);
        match manager.diagnostics(&path).await {
            Ok(diagnostics) => {
                let _sent = reports.try_send(Report::checked(
                    display,
                    server,
                    diagnostics.iter().map(convert).collect(),
                ));
            }
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "lsp diagnostics failed");
                let _sent = reports.try_send(Report::unchecked(display));
            }
        }
    }
}

/// Flatten one LSP diagnostic for display.
///
/// The `+ 1` on both axes is the whole reason this function exists: LSP positions are
/// zero-based and every editor a user pastes a location into is one-based, so a report
/// that passed them through would be off by one everywhere.
fn convert(diagnostic: &zuno_lsp::client::Diagnostic) -> Diagnostic {
    Diagnostic {
        severity: Severity::from_lsp(diagnostic.severity),
        line: diagnostic.range.start.line.saturating_add(1),
        column: diagnostic.range.start.character.saturating_add(1),
        source: diagnostic.source.clone(),
        // A server may send a multi-line message; a transcript row is one line.
        message: diagnostic
            .message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    }
}

#[cfg(test)]
#[path = "tui_lsp_tests.rs"]
mod tests;

/// The manager and workspace one session's checks run against.
///
/// One value so [`check_edits`] takes a single `Option` rather than two that have to
/// agree, and so a host that cannot build a manager — every server disabled, which is
/// the default — passes `None` and the task degrades to doing nothing.
pub(super) struct Probe {
    manager: Arc<Manager>,
    workspace: PathBuf,
    /// How the render loop is told a report is waiting.
    ///
    /// Without it the report sits in its channel until the *next* unrelated event, which
    /// after a finished turn may be never: the loop only drains on an event, and a
    /// completed turn is the last one it will see. That is the same reason
    /// `PermissionBroker` nudges — a channel whose consumer only runs on somebody else's
    /// event is not actually connected.
    wake: mpsc::Sender<zuno_tui::app::TerminalEvent>,
}

impl Probe {
    /// A probe over the enabled servers `config` leaves, or `None` when there are none.
    ///
    /// `None` rather than an empty manager because a manager with no servers would answer
    /// every file as unchecked, and the transcript would then carry a row per edit saying
    /// so — noise for the many users who have no `lsp` key at all.
    pub(super) fn resolve(
        config: &zuno_config::schema::Config,
        workspace: &Path,
        wake: mpsc::Sender<zuno_tui::app::TerminalEvent>,
    ) -> Option<Self> {
        let resolved = zuno_catalog::lsp_config::ResolvedLsp::resolve(config.lsp.as_ref());
        let registry = zuno_lsp::registry::ServerRegistry::offline(&resolved);
        if registry.servers().is_empty() {
            return None;
        }
        Some(Self {
            manager: Arc::new(Manager::new(
                workspace,
                Arc::new(registry),
                zuno_lsp::manager::RestartPolicy::default(),
            )),
            workspace: workspace.to_path_buf(),
            wake,
        })
    }
}

/// Check every batch of written files until the screen stops sending.
///
/// A task of its own, and the reason is the same one that keeps a turn out of the render
/// loop: `Manager::diagnostics` spawns a language server and waits for it to index, which
/// is seconds on a cold cache. Nothing the loop touches may await that.
pub(super) async fn check_edits(
    probe: Option<Probe>,
    mut edits: mpsc::Receiver<Vec<String>>,
    reports: mpsc::Sender<Report>,
) {
    let Some(probe) = probe else {
        // Drained rather than dropped: a closed receiver would make the screen's
        // `try_send` fail, and a failure it reports nowhere is worse than a no-op.
        while edits.recv().await.is_some() {}
        return;
    };
    while let Some(batch) = edits.recv().await {
        let paths = batch
            .into_iter()
            .map(|path| {
                let candidate = PathBuf::from(&path);
                if candidate.is_absolute() {
                    candidate
                } else {
                    probe.workspace.join(candidate)
                }
            })
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        if paths.is_empty() {
            continue;
        }
        report(
            Arc::clone(&probe.manager),
            probe.workspace.clone(),
            paths,
            reports.clone(),
        )
        .await;
        // Nudge *after* the reports are queued, so the frame the loop draws includes them.
        let _nudged = probe.wake.try_send(zuno_tui::app::TerminalEvent::Wake);
    }
    probe.manager.shutdown().await;
}

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

use futures::stream::{self, StreamExt as _};
use tokio::sync::{mpsc, watch};
use zuno_lsp::manager::Manager;
use zuno_tui::views::lsp::{Diagnostic, PendingEditReader, Report, Severity};

/// How many reports may be queued.
///
/// One per file in a turn's edit set, and a turn that edits more than this has already
/// given the user more than they can read; a full channel costs a report, never a stall.
pub(super) const REPORT_CHANNEL_CAPACITY: usize = 16;

/// Ask `manager` about each of `paths` and send one report per file.
///
/// Returns `false` once the screen is gone, so the caller stops asking language servers
/// about files nobody will be shown.
///
/// # Why every send is awaited
///
/// These used to be `try_send` with the outcome discarded, and a turn with a larger edit
/// set than the channel silently lost the overflow: seventeen writes against a sixteen-slot
/// channel showed sixteen results and looked complete. That breaks this module's own
/// contract — an unclaimed file must be *reported* as unclaimed, because silence on it
/// reads as a clean bill of health.
///
/// `try_send`'s refuse-newest is right for a value the producer can regenerate and wrong
/// for a finding nobody can. There is no stall to fear: this runs in its own task, and the
/// only thing awaiting a slot is the next language-server query — work whose result has
/// nowhere to go until the screen has read what is already queued.
async fn report(
    manager: &Arc<Manager>,
    workspace: &Path,
    paths: Vec<PathBuf>,
    reports: &mpsc::Sender<Report>,
    wake: &mpsc::Sender<zuno_tui::app::TerminalEvent>,
) -> bool {
    let checks = stream::iter(paths.into_iter().map(|path| {
        let manager = Arc::clone(manager);
        let workspace = workspace.to_path_buf();
        async move { check_path(&manager, &workspace, path).await }
    }))
    .buffered(manager.request_concurrency().get());
    tokio::pin!(checks);
    while let Some(report) = checks.next().await {
        if !deliver(report, reports, wake).await {
            return false;
        }
    }
    true
}

async fn check_path(manager: &Manager, workspace: &Path, path: PathBuf) -> Report {
    let display = path
        .strip_prefix(workspace)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    if !manager.has_server(&path) {
        return Report::unchecked(display);
    }
    match manager.diagnostics(&path).await {
        Ok(diagnostics) => {
            let server = manager
                .status()
                .await
                .into_iter()
                .find(|status| path.starts_with(&status.root))
                .map_or_else(|| String::from("lsp"), |status| status.id);
            Report::checked(display, server, diagnostics.iter().map(convert).collect())
        }
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "lsp diagnostics failed");
            Report::unchecked(display)
        }
    }
}

/// Hand one report over, waiting for room, and nudge the loop to read it.
///
/// The nudge is per report rather than per batch, and that is load-bearing rather than
/// eager: the loop drains only when woken, so a batch larger than the channel would have
/// the producer waiting for room that only a drain can make and the loop waiting for a
/// wake that only the finished batch would send. Each side would be waiting for the other.
///
/// `false` means either channel's receiver is closed — the screen or the loop has gone —
/// which is the one case where giving up is correct, and it terminates the caller rather
/// than being ignored.
async fn deliver(
    report: Report,
    reports: &mpsc::Sender<Report>,
    wake: &mpsc::Sender<zuno_tui::app::TerminalEvent>,
) -> bool {
    let path = report.path.clone();
    if reports.send(report).await.is_err() {
        tracing::debug!(%path, "the screen closed before its diagnostics were read");
        return false;
    }
    // Awaited, like the report itself. This was `try_send` on the reasoning that a full
    // queue must already hold a "look again" the loop has not read — which is false: the
    // sixty-four slots are shared with every keystroke and resize, so a burst of typing
    // fills them with events that say nothing about diagnostics. The report was then
    // queued and the only thing that would have drawn it was gone.
    //
    // Queueing the report is not enough on its own. The screen drains reports when it
    // handles an event, and nothing guarantees another arrives: `SessionScreen`'s handler
    // returns early for paste and printable keys, so "some later event will drain it" is
    // not a backstop. The nudge is the guarantee.
    //
    // Waiting cannot deadlock: the event loop receives terminal events unconditionally,
    // including while a lease is held, so a slot always comes free.
    if wake.send(zuno_tui::app::TerminalEvent::Wake).await.is_err() {
        // The one case where giving up is right — no loop remains to show anything.
        tracing::debug!(%path, "the event loop closed before it could be nudged");
        return false;
    }
    true
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
                std::num::NonZeroUsize::new(usize::from(
                    config.resolved_concurrency().lsp_requests,
                ))
                .expect("configuration validates LSP concurrency"),
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
    pending: PendingEditReader,
    mut signals: mpsc::Receiver<()>,
    reports: mpsc::Sender<Report>,
    mut shutdown: watch::Receiver<bool>,
) {
    let Some(probe) = probe else {
        // Drained rather than dropped, so the set does not grow for the lifetime of a
        // session nobody is checking. No truncation notice here and that is deliberate:
        // with no server enabled — the default — nothing was going to be checked anyway,
        // so saying a subset went unchecked would be a row per turn about nothing.
        loop {
            tokio::select! {
                signal = signals.recv() => match signal {
                    Some(()) => {
                        let _discarded = pending.take();
                    }
                    None => return,
                },
                changed = shutdown.changed() => {
                    let _changed = changed;
                    return;
                }
            }
        }
    };
    loop {
        let signalled = tokio::select! {
            signal = signals.recv() => signal.is_some(),
            changed = shutdown.changed() => {
                let _changed = changed;
                false
            }
        };
        if !signalled {
            break;
        }
        // Everything waiting, not the one batch a message carried. A nudge lost to a full
        // capacity-one channel therefore costs nothing: this drain finds what it would
        // have carried, which is the whole reason the paths do not travel as messages.
        let (batch, overflowed) = pending.take();
        if overflowed > 0 {
            // On screen, not only in the log. The default TUI shows no log, so a
            // `tracing::warn!` alone left a truncated check looking like a complete one —
            // the same "silence reads as a clean bill of health" this module refuses for a
            // file no server claims. It goes first, before the reports it qualifies.
            tracing::warn!(
                overflowed,
                limit = zuno_tui::views::lsp::PENDING_EDIT_LIMIT,
                "more files were written than the pending-edit set holds; some are unchecked"
            );
            if !deliver(
                Report::truncated(overflowed, zuno_tui::views::lsp::PENDING_EDIT_LIMIT),
                &reports,
                &probe.wake,
            )
            .await
            {
                break;
            }
        }
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
        // Each report nudges as it lands — see [`deliver`]. Nudging only once the whole
        // batch was queued is what made a batch larger than the channel unable to drain.
        if !report(
            &probe.manager,
            &probe.workspace,
            paths,
            &reports,
            &probe.wake,
        )
        .await
        {
            break;
        }
    }
    probe.manager.shutdown().await;
}

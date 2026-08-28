//! MCP bring-up for the two surfaces with no screen attached.
//!
//! # Why this exists rather than living in each command
//!
//! MCP was wired into exactly one of three surfaces. `zuno tui` built a
//! [`zuno_mcp::Catalog`] and a [`zuno_mcp::McpServerController`] and handed the
//! catalog to [`super::turn::TurnHost::open_with_runtime_and_mcp`]; `zuno run` and
//! `zuno serve` reached the same constructor through
//! [`super::turn::TurnHost::open_with_runtime`], which passes `None`. The result was
//! silent rather than broken: the same configuration produced a working MCP tool in
//! the TUI and **zero** MCP tools headlessly, with nothing said either way.
//!
//! Both headless surfaces now come through here, for the reason
//! [`super::tool_runtime`] gives for tool assembly: a second bring-up site is how the
//! surfaces diverge again.
//!
//! # Connect-then-open, rather than the TUI's connect-then-rebuild
//!
//! The registry reads [`zuno_tools::registry::McpToolLoader::tools`] once, while it is
//! being built, so a server that finishes connecting after the host is open
//! contributes nothing to that host. The TUI answers this by rebuilding the host on a
//! dirty flag before the next turn — correct there, because it has many turns and a
//! render loop to hang the retry on.
//!
//! Neither headless surface does. `zuno run` has exactly one turn, so a late
//! connection has no later turn to appear in; `zuno serve` opens a fresh host per
//! request, so it re-reads the catalog anyway. So the wait happens **before** the
//! host is built, bounded by [`zuno_mcp::McpLifecycleOptions`]'s own timeouts, and a
//! server that fails is reported as a note rather than failing the run — a broken MCP
//! server must not make `zuno run` unusable.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::path::Path;

use futures::stream::{self, StreamExt as _};
use serde::Serialize;
use zuno_config::schema::Config;
use zuno_config::schema::mcp::McpServerConfig;
use zuno_orchestration::ToolSchemaIdentity;

/// One configured MCP server after an active diagnostic connection attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpServerDiagnostic {
    pub(crate) name: String,
    pub(crate) desired_enabled: bool,
    pub(crate) state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// Live MCP facts collected by `zuno debug agent`.
///
/// This is deliberately produced by the same runtime used by `run` and `serve`.
/// A diagnostic command must not claim an MCP tool is available merely because a
/// server appears in configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpRuntimeDiagnostics {
    pub(crate) discovery_status: String,
    pub(crate) servers: Vec<McpServerDiagnostic>,
    pub(crate) connected_servers: Vec<String>,
    pub(crate) tools: Vec<ToolSchemaIdentity>,
    pub(crate) warnings: Vec<String>,
    pub(crate) cleanup_warnings: Vec<String>,
}

/// Whether a configured server is one this surface should connect at startup.
///
/// `zuno tui` carries a private copy of this predicate (`tui.rs:913`). Collapse the
/// two the next time that file is touched: three surfaces disagreeing about which
/// servers are enabled is the same defect class this module closes.
pub(crate) fn enabled(server: &McpServerConfig) -> bool {
    match server {
        McpServerConfig::Local(local) => local.enabled.unwrap_or(true),
        McpServerConfig::Remote(remote) => remote.enabled.unwrap_or(true),
        McpServerConfig::Toggle(toggle) => toggle.enabled,
    }
}

/// A connected MCP catalog and the controller that owns its transports.
pub(crate) struct McpRuntime {
    catalog: zuno_mcp::Catalog,
    controller: zuno_mcp::McpServerController,
    enabled: Vec<String>,
    concurrency: NonZeroUsize,
}

impl McpRuntime {
    /// Build a runtime for the servers `config` declares, or [`None`] for none.
    ///
    /// [`None`] rather than an empty runtime so a caller with no MCP configuration
    /// spawns no controller and pays nothing — and so the `mcp` argument the host
    /// takes stays `None` in exactly the case it always was.
    pub(crate) fn from_config(config: &Config, workspace: &Path) -> Option<Self> {
        let configs: BTreeMap<String, McpServerConfig> = config
            .mcp
            .as_ref()?
            .iter()
            .map(|(name, server)| ((*name).to_owned(), server.clone()))
            .collect();
        if configs.is_empty() {
            return None;
        }
        let enabled = configs
            .iter()
            .filter(|(_, server)| self::enabled(server))
            .map(|(name, _)| name.clone())
            .collect();
        let catalog = zuno_mcp::Catalog::new(configs.keys().cloned());
        let controller = zuno_mcp::McpServerController::from_config(
            catalog.clone(),
            workspace,
            configs,
            zuno_mcp::McpLifecycleOptions::default(),
        );
        Some(Self {
            catalog,
            controller,
            enabled,
            concurrency: NonZeroUsize::new(usize::from(
                config.resolved_concurrency().mcp_connections,
            ))
            .expect("configuration validates MCP concurrency"),
        })
    }

    /// Connect every enabled server, returning one note per server that did not.
    ///
    pub(crate) async fn connect(&self) -> Vec<String> {
        let results = stream::iter(self.enabled.iter().cloned().map(|server| {
            let controller = self.controller.clone();
            async move {
                let result = controller.set_enabled(&server, true).await;
                (server, result)
            }
        }))
        .buffered(self.concurrency.get())
        .collect::<Vec<_>>()
        .await;
        results
            .into_iter()
            .filter_map(|(server, result)| match result {
                Ok(snapshot) => match snapshot.state {
                    zuno_mcp::McpServerState::Failed { error }
                    | zuno_mcp::McpServerState::NeedsClientRegistration { error } => {
                        Some(format!("warning: MCP server `{server}` failed: {error}"))
                    }
                    zuno_mcp::McpServerState::NeedsAuth => Some(format!(
                        "warning: MCP server `{server}` needs authorization completed \
                         before its tools are available; run `zuno mcp` to authorize it"
                    )),
                    zuno_mcp::McpServerState::Connected
                    | zuno_mcp::McpServerState::Connecting
                    | zuno_mcp::McpServerState::Disconnecting
                    | zuno_mcp::McpServerState::Disabled => None,
                },
                Err(error) => Some(format!("warning: MCP server `{server}` failed: {error}")),
            })
            .collect()
    }

    /// The catalog to hand a host.
    pub(crate) fn catalog(&self) -> zuno_mcp::Catalog {
        self.catalog.clone()
    }

    /// Snapshot the connected catalog and exact provider-visible tool schemas.
    pub(crate) fn diagnostics(&self, warnings: Vec<String>) -> McpRuntimeDiagnostics {
        let discovery_status = match self.catalog.discovery_status() {
            zuno_llm::cache::McpToolStatus::Pending => "pending",
            zuno_llm::cache::McpToolStatus::Ready => "ready",
        }
        .to_owned();
        let servers = self
            .controller
            .snapshots()
            .into_iter()
            .map(|snapshot| {
                let (state, error) = state_diagnostic(&snapshot.state);
                McpServerDiagnostic {
                    name: snapshot.server,
                    desired_enabled: snapshot.desired_enabled,
                    state: state.to_owned(),
                    error,
                }
            })
            .collect();
        let tools = self
            .catalog
            .tools()
            .into_iter()
            .map(|tool| tool.definition().schema_identity())
            .collect();
        McpRuntimeDiagnostics {
            discovery_status,
            servers,
            connected_servers: self.catalog.connected_servers(),
            tools,
            warnings,
            cleanup_warnings: Vec::new(),
        }
    }

    /// Close every transport, waiting for each.
    ///
    /// Dropping the controller would abort the transport tasks and let `Drop` signal
    /// any subprocess, which the TUI relies on (`tui.rs:477`). That leaves a remote
    /// server's HTTP session open on the far side, because only
    /// [`zuno_mcp::McpConnection::close`] deletes it. A headless surface has an exit
    /// it can await, so it awaits.
    pub(crate) async fn shutdown(self) {
        let _warnings = self.shutdown_with_diagnostics().await;
    }

    /// Close every transport and return any cleanup failures for diagnostics.
    pub(crate) async fn shutdown_with_diagnostics(self) -> Vec<String> {
        let connected = self
            .catalog
            .connected_servers()
            .into_iter()
            .collect::<BTreeSet<_>>();
        stream::iter(self.enabled.into_iter().map(|server| {
            let controller = self.controller.clone();
            let was_connected = connected.contains(&server);
            async move {
                let result = controller.set_enabled(&server, false).await;
                (server, was_connected, result)
            }
        }))
        .buffered(self.concurrency.get())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .filter_map(|(server, was_connected, result)| match result {
            Ok(snapshot) if was_connected => match snapshot.state {
                zuno_mcp::McpServerState::Failed { error }
                | zuno_mcp::McpServerState::NeedsClientRegistration { error } => Some(format!(
                    "warning: MCP server `{server}` cleanup failed: {error}"
                )),
                zuno_mcp::McpServerState::NeedsAuth => Some(format!(
                    "warning: MCP server `{server}` entered authorization state during cleanup"
                )),
                zuno_mcp::McpServerState::Disabled => None,
                zuno_mcp::McpServerState::Connecting => Some(format!(
                    "warning: MCP server `{server}` remained connecting after cleanup"
                )),
                zuno_mcp::McpServerState::Connected => Some(format!(
                    "warning: MCP server `{server}` remained connected after cleanup"
                )),
                zuno_mcp::McpServerState::Disconnecting => Some(format!(
                    "warning: MCP server `{server}` remained disconnecting after cleanup"
                )),
            },
            Ok(_) => None,
            Err(error) => Some(format!(
                "warning: MCP server `{server}` cleanup failed: {error}"
            )),
        })
        .collect()
    }
}

fn state_diagnostic(state: &zuno_mcp::McpServerState) -> (&'static str, Option<String>) {
    match state {
        zuno_mcp::McpServerState::Disabled => ("disabled", None),
        zuno_mcp::McpServerState::Connecting => ("connecting", None),
        zuno_mcp::McpServerState::Connected => ("connected", None),
        zuno_mcp::McpServerState::Disconnecting => ("disconnecting", None),
        zuno_mcp::McpServerState::Failed { error } => ("failed", Some(error.clone())),
        zuno_mcp::McpServerState::NeedsAuth => ("needs-auth", None),
        zuno_mcp::McpServerState::NeedsClientRegistration { error } => {
            ("needs-client-registration", Some(error.clone()))
        }
    }
}

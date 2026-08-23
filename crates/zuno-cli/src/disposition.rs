//! One explicit decision for every command registered by upstream 1.18.13.
//!
//! The table is keyed by the TypeScript symbol, not only by its CLI spelling.
//! That lets a mechanically extracted fixture from `index.ts:45-103` detect a new
//! registration even when it reuses an alias or default command name.

use std::collections::BTreeSet;

/// What the Rust command tree does with an upstream registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Registered now and routed through the command-dispatch seam.
    Implemented,
    /// Registered now so invoking it produces a deliberate migration message.
    Rejected,
    /// Deliberately absent until its named owner can provide an honest handler.
    NotRegistered,
}

/// The decision for one upstream `*Command` symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDisposition {
    /// Symbol extracted from `packages/opencode/src/index.ts`.
    pub upstream_symbol: &'static str,
    /// User-facing command spelling.
    ///
    /// `TuiThreadCommand` is upstream's default command with no name of its own;
    /// this port gives it the explicit spelling `tui` and *also* accepts the bare
    /// invocation, so the row names `tui` and the registration check can find it.
    pub command: &'static str,
    /// Whether and how this crate registers it.
    pub disposition: Disposition,
    /// Why this is the honest disposition and who owns the replacement.
    pub reason: &'static str,
}

const COMMAND_DISPOSITIONS: [CommandDisposition; 23] = [
    CommandDisposition {
        upstream_symbol: "AcpCommand",
        command: "acp",
        disposition: Disposition::NotRegistered,
        reason: "todo 78 owns the zuno-acp protocol adapter; registering it before that handler exists would advertise a server that cannot speak ACP",
    },
    CommandDisposition {
        upstream_symbol: "AgentCommand",
        command: "agent",
        disposition: Disposition::Implemented,
        reason: "registered through the headless-command seam for todo 56",
    },
    CommandDisposition {
        upstream_symbol: "AttachCommand",
        command: "attach",
        disposition: Disposition::NotRegistered,
        reason: "attach requires the TUI client and terminal lifecycle owned by the TUI wave; no headless substitute is equivalent",
    },
    CommandDisposition {
        upstream_symbol: "ConsoleCommand",
        command: "console",
        disposition: Disposition::Rejected,
        reason: "Zuno does not provide a hosted console; use `providers` (alias `auth`) for local credentials instead",
    },
    CommandDisposition {
        upstream_symbol: "DbCommand",
        command: "db",
        disposition: Disposition::Implemented,
        reason: "registered through the headless-command seam for todo 56 and the maintenance extensions in todo 84",
    },
    CommandDisposition {
        upstream_symbol: "DebugCommand",
        command: "debug",
        disposition: Disposition::Implemented,
        reason: "registered through the headless-command seam for todo 56",
    },
    CommandDisposition {
        upstream_symbol: "ExportCommand",
        command: "export",
        disposition: Disposition::Implemented,
        reason: "prints one session's whole transcript as JSON, byte-compared against the released binary's own export, with `--sanitize` redacting the same fields",
    },
    CommandDisposition {
        upstream_symbol: "GenerateCommand",
        command: "generate",
        disposition: Disposition::Rejected,
        reason: "the command is a TypeScript source-tree SDK/OpenAPI generator that depends on Prettier and is excluded from the runtime binary; use the server's `/openapi.json` document instead",
    },
    CommandDisposition {
        upstream_symbol: "GithubCommand",
        command: "github",
        disposition: Disposition::Rejected,
        reason: "the hosted GitHub agent is outside the local-agent scope; run `zuno run` from the CI workflow instead",
    },
    CommandDisposition {
        upstream_symbol: "ImportCommand",
        command: "import",
        disposition: Disposition::Implemented,
        reason: "reads a local `export` document into Zuno's database; share-URL imports are not accepted because Zuno does not integrate with the hosted share service",
    },
    CommandDisposition {
        upstream_symbol: "McpCommand",
        command: "mcp",
        disposition: Disposition::Implemented,
        reason: "registered through the headless-command seam for todo 56",
    },
    CommandDisposition {
        upstream_symbol: "ModelsCommand",
        command: "models",
        disposition: Disposition::Implemented,
        reason: "registered through the headless-command seam for todo 56",
    },
    CommandDisposition {
        upstream_symbol: "PluginCommand",
        command: "plugin",
        disposition: Disposition::Implemented,
        reason: "lists, installs, updates, and removes native Zuno plugin packages through the local extension host",
    },
    CommandDisposition {
        upstream_symbol: "PrCommand",
        command: "pr",
        disposition: Disposition::Rejected,
        reason: "the GitHub checkout helper is excluded from the local-agent runtime; use `gh pr checkout <number>` and then `zuno run` instead",
    },
    CommandDisposition {
        upstream_symbol: "ProvidersCommand",
        command: "providers",
        disposition: Disposition::Implemented,
        reason: "registered with the upstream `auth` alias through the headless-command seam for todo 56",
    },
    CommandDisposition {
        upstream_symbol: "RunCommand",
        command: "run",
        disposition: Disposition::Implemented,
        reason: "registered through the headless-command seam for todo 56",
    },
    CommandDisposition {
        upstream_symbol: "ServeCommand",
        command: "serve",
        disposition: Disposition::Implemented,
        reason: "registered through the headless-command seam; todo 56 wraps zuno-server's public builder rather than duplicating its server logic",
    },
    CommandDisposition {
        upstream_symbol: "SessionCommand",
        command: "session",
        disposition: Disposition::Implemented,
        reason: "registered through the headless-command seam for todo 56 and session maintenance todos 80-85",
    },
    CommandDisposition {
        upstream_symbol: "StatsCommand",
        command: "stats",
        disposition: Disposition::Rejected,
        reason: "upstream stats reads the excluded stats package's session SQL directly; use `db stats` from todo 84 instead",
    },
    CommandDisposition {
        upstream_symbol: "TuiThreadCommand",
        command: "tui",
        disposition: Disposition::Implemented,
        reason: "registered as `tui` and as the bare invocation upstream spells `$0`; it boots zuno-tui's application over the terminal lease from todo 73 and the views from todo 76",
    },
    CommandDisposition {
        upstream_symbol: "UninstallCommand",
        command: "uninstall",
        disposition: Disposition::Rejected,
        reason: "self-uninstallation is excluded from the runtime; remove `zuno` with the package manager or installer that placed it",
    },
    CommandDisposition {
        upstream_symbol: "UpgradeCommand",
        command: "self-update",
        disposition: Disposition::Implemented,
        reason: "adapted as a native Rust updater that selects Zuno release assets, verifies SHA256SUMS, and atomically replaces the running executable",
    },
    CommandDisposition {
        upstream_symbol: "WebCommand",
        command: "web",
        disposition: Disposition::Rejected,
        reason: "the bundled hosted web application is excluded from this headless Rust scope; use `serve` and connect a supported client instead",
    },
];

/// Every upstream registration and its one deliberate outcome.
#[must_use]
pub const fn dispositions() -> &'static [CommandDisposition] {
    &COMMAND_DISPOSITIONS
}

/// Why the frozen upstream fixture and the disposition table disagree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SurfaceError {
    /// The extraction itself contains the same symbol twice.
    #[error("upstream command fixture contains duplicate entry `{symbol}`")]
    DuplicateFixture { symbol: String },
    /// A registered upstream command has no decision.
    #[error("upstream command `{symbol}` has no disposition")]
    MissingDisposition { symbol: String },
    /// Two table rows claim the same upstream symbol.
    #[error("upstream command `{symbol}` has {count} dispositions; expected exactly one")]
    DuplicateDisposition { symbol: String, count: usize },
    /// A stale row remains after upstream stopped registering the command.
    #[error("disposition table contains `{symbol}`, which is absent from the upstream fixture")]
    StaleDisposition { symbol: String },
}

/// Proves the fixture and table form a one-to-one mapping.
///
/// This checks both directions. Checking only fixture → table would allow a stale
/// disposition to make an accidentally removed registration look covered.
pub fn validate_upstream_surface(fixture: &str) -> Result<(), SurfaceError> {
    let mut upstream = BTreeSet::new();
    for line in fixture
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !upstream.insert(line) {
            return Err(SurfaceError::DuplicateFixture {
                symbol: line.to_owned(),
            });
        }
    }

    for symbol in &upstream {
        let count = COMMAND_DISPOSITIONS
            .iter()
            .filter(|entry| entry.upstream_symbol == *symbol)
            .count();
        match count {
            0 => {
                return Err(SurfaceError::MissingDisposition {
                    symbol: (*symbol).to_owned(),
                });
            }
            1 => {}
            _ => {
                return Err(SurfaceError::DuplicateDisposition {
                    symbol: (*symbol).to_owned(),
                    count,
                });
            }
        }
    }

    if let Some(stale) = COMMAND_DISPOSITIONS
        .iter()
        .find(|entry| !upstream.contains(entry.upstream_symbol))
    {
        return Err(SurfaceError::StaleDisposition {
            symbol: stale.upstream_symbol.to_owned(),
        });
    }
    Ok(())
}

/// Finds the decision behind one registered spelling.
#[must_use]
pub fn disposition_for(command: &str) -> Option<&'static CommandDisposition> {
    COMMAND_DISPOSITIONS
        .iter()
        .find(|entry| entry.command == command)
}

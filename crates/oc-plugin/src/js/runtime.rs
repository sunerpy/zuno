//! Which JavaScript runtime runs the compat host, and what to say when there is none.
//!
//! `bun` is preferred and `node` is the fallback, in that order, because
//! `PluginInput.$` is a Bun shell (`packages/plugin/src/index.ts:65`). Under node
//! that field cannot be honoured, so a plugin that shells out fails with an
//! explanation instead of a `TypeError` from inside its own bundle.
//!
//! Nothing here bundles a runtime. Discovery is a `PATH` walk, and the absence of
//! both is a *diagnostic naming the affected plugins*, not a panic and not a
//! silent skip — a user whose auth plugins vanished needs to be told why.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// A JavaScript runtime that can host plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JsRuntimeKind {
    /// Preferred: provides the real `PluginInput.$` Bun shell.
    Bun,
    /// Fallback: everything except `$` works.
    Node,
}

impl JsRuntimeKind {
    /// Discovery order. `bun` first; see the module note.
    pub const PREFERENCE: [Self; 2] = [Self::Bun, Self::Node];

    /// The executable name looked up on `PATH`.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Node => "node",
        }
    }

    /// Flags that bound this runtime's heap, given a ceiling in bytes.
    ///
    /// The two runtimes disagree about what a memory limit even is. Node takes a
    /// hard old-space ceiling in MiB. Bun has no equivalent: `--smol` only trades
    /// throughput for a smaller resident set. Neither is sufficient on its own,
    /// which is why [`crate::js::MemoryCeiling`] also samples RSS and restarts —
    /// the flags reduce the pressure, the sampler enforces the promise.
    #[must_use]
    pub fn memory_flags(self, ceiling_bytes: u64) -> Vec<OsString> {
        match self {
            Self::Bun => vec![OsString::from("--smol")],
            Self::Node => {
                let mib = (ceiling_bytes / (1024 * 1024)).max(64);
                vec![OsString::from(format!("--max-old-space-size={mib}"))]
            }
        }
    }
}

impl fmt::Display for JsRuntimeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.program())
    }
}

/// A discovered runtime: which one, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsRuntime {
    kind: JsRuntimeKind,
    program: PathBuf,
}

impl JsRuntime {
    /// Name a runtime directly, for tests and for an explicit user override.
    #[must_use]
    pub fn new(kind: JsRuntimeKind, program: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            program: program.into(),
        }
    }

    /// Which runtime this is.
    #[must_use]
    pub const fn kind(&self) -> JsRuntimeKind {
        self.kind
    }

    /// The executable to spawn.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }
}

impl fmt::Display for JsRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.kind, self.program.display())
    }
}

/// No JavaScript runtime is installed, and these plugins therefore cannot load.
///
/// The plugin list is carried rather than logged separately so that one message
/// answers both "why is my provider missing" and "what do I install".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "no JavaScript runtime found on PATH (looked for {searched}); \
     these plugins cannot load: {plugins}. Install `bun` (preferred) or `node`."
)]
pub struct MissingJsRuntime {
    searched: String,
    plugins: String,
}

impl MissingJsRuntime {
    fn new(plugins: &[String]) -> Self {
        let plugins = if plugins.is_empty() {
            "(none configured)".to_owned()
        } else {
            plugins.join(", ")
        };
        Self {
            searched: JsRuntimeKind::PREFERENCE
                .map(JsRuntimeKind::program)
                .join(", "),
            plugins,
        }
    }

    /// The affected plugin names, as rendered into the message.
    #[must_use]
    pub fn plugins(&self) -> &str {
        &self.plugins
    }
}

/// Find the best available runtime, naming `plugins` if none exists.
///
/// # Errors
/// Returns [`MissingJsRuntime`] when neither `bun` nor `node` is on `PATH`.
pub fn discover_runtime(plugins: &[String]) -> Result<JsRuntime, MissingJsRuntime> {
    discover_runtime_in(env::var_os("PATH").as_deref(), plugins)
}

/// `discover_runtime` against an explicit `PATH`, so a test can simulate absence.
///
/// Simulating absence is the only way to test the diagnostic on a machine that has
/// both runtimes installed, and the test suite must not depend on a machine that
/// lacks them.
///
/// # Errors
/// Returns [`MissingJsRuntime`] when neither runtime is found in `path`.
pub fn discover_runtime_in(
    path: Option<&std::ffi::OsStr>,
    plugins: &[String],
) -> Result<JsRuntime, MissingJsRuntime> {
    let directories: Vec<PathBuf> = path
        .map(|path| env::split_paths(path).collect())
        .unwrap_or_default();
    for kind in JsRuntimeKind::PREFERENCE {
        for directory in &directories {
            let candidate = directory.join(kind.program());
            if is_executable_file(&candidate) {
                return Ok(JsRuntime::new(kind, candidate));
            }
        }
    }
    Err(MissingJsRuntime::new(plugins))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| !metadata.is_dir() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    // Windows has no execute bit; the extension is the only signal, and both
    // runtimes ship as `.exe`. `bun`/`node` with no extension is also accepted
    // because a shim may be a `.cmd` resolved by the shell.
    std::fs::metadata(path).is_ok_and(|metadata| !metadata.is_dir())
        || std::fs::metadata(path.with_extension("exe")).is_ok_and(|m| !m.is_dir())
}

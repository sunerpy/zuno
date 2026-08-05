//! Choosing between the embedded engine and a system `rg`.
//!
//! The embedded engine is the default and the reason the runtime download in
//! `packages/core/src/ripgrep/binary.ts:88-121` is gone: nothing in this port fetches
//! a 3 MB archive from GitHub on first search, extracts it with `tar` or
//! `powershell.exe`, and `chmod`s the result. A `rg` on the host is used only when
//! explicitly asked for, so a machine with an old `rg` on `PATH` cannot silently
//! change what search returns.

use crate::cancel::Cancellation;
use crate::embedded::EmbeddedEngine;
use crate::error::SearchError;
use crate::ripgrep::{RipgrepEngine, locate_ripgrep};
use crate::types::{Entry, GlobRequest, GrepRequest, Match, SearchResults};
use std::path::PathBuf;

/// The environment variable that opts into the `rg` backend.
///
/// Accepts `ripgrep` or `rg`. Any other value, including absent, selects the
/// embedded engine. Naming it explicitly means a divergence investigation can switch
/// backends without a rebuild.
pub const BACKEND_ENV: &str = "OPENCODE_SEARCH_BACKEND";

/// Which engine answers a request.
#[derive(Debug, Clone)]
pub enum Backend {
    /// `ignore` + `grep-searcher`, in process.
    Embedded(EmbeddedEngine),
    /// A `rg` binary on the host.
    Ripgrep(RipgrepEngine),
}

impl Default for Backend {
    fn default() -> Self {
        Self::Embedded(EmbeddedEngine)
    }
}

impl Backend {
    /// The embedded engine.
    #[must_use]
    pub fn embedded() -> Self {
        Self::Embedded(EmbeddedEngine)
    }

    /// A `rg` binary at an explicit path.
    #[must_use]
    pub fn ripgrep(program: impl Into<PathBuf>) -> Self {
        Self::Ripgrep(RipgrepEngine::new(program))
    }

    /// Reads [`BACKEND_ENV`] and resolves it.
    ///
    /// Falls back to the embedded engine when `ripgrep` was asked for but no `rg` is
    /// on `PATH`, because the alternative — failing every search — is worse than
    /// serving it from an engine that produces the same answers. The fallback is
    /// logged at warn so it is not silent.
    #[must_use]
    pub fn from_env() -> Self {
        Self::select(std::env::var(BACKEND_ENV).ok().as_deref())
    }

    /// The pure form of [`Backend::from_env`].
    ///
    /// Split out because a test cannot set an environment variable in this
    /// workspace: Rust 2024 makes `std::env::set_var` `unsafe` and the workspace
    /// forbids `unsafe_code`.
    #[must_use]
    pub fn select(requested: Option<&str>) -> Self {
        match requested.map(str::trim) {
            Some("ripgrep" | "rg") => match locate_ripgrep() {
                Some(program) => Self::ripgrep(program),
                None => {
                    tracing::warn!(
                        env = BACKEND_ENV,
                        "the ripgrep backend was requested but no rg binary is on PATH; \
                         using the embedded engine"
                    );
                    Self::embedded()
                }
            },
            _ => Self::embedded(),
        }
    }

    /// A stable name for the selected backend, for metadata and evidence.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Embedded(_) => "embedded",
            Self::Ripgrep(_) => "ripgrep",
        }
    }

    /// Lists files matching a glob.
    ///
    /// # Errors
    ///
    /// Whatever the selected engine reports; see [`EmbeddedEngine::glob`] and
    /// [`RipgrepEngine::glob`].
    pub fn glob(
        &self,
        request: &GlobRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Entry>, SearchError> {
        match self {
            Self::Embedded(engine) => engine.glob(request, cancel),
            Self::Ripgrep(engine) => engine.glob(request, cancel),
        }
    }

    /// Searches file contents for a regex.
    ///
    /// # Errors
    ///
    /// Whatever the selected engine reports; see [`EmbeddedEngine::grep`] and
    /// [`RipgrepEngine::grep`].
    pub fn grep(
        &self,
        request: &GrepRequest,
        cancel: &dyn Cancellation,
    ) -> Result<SearchResults<Match>, SearchError> {
        match self {
            Self::Embedded(engine) => engine.grep(request, cancel),
            Self::Ripgrep(engine) => engine.grep(request, cancel),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_engine_is_the_default_and_needs_no_binary() {
        assert_eq!(Backend::default().name(), "embedded");
        assert_eq!(Backend::select(None).name(), "embedded");
        assert_eq!(Backend::select(Some("")).name(), "embedded");
        assert_eq!(Backend::select(Some("embedded")).name(), "embedded");
        assert_eq!(Backend::select(Some("nonsense")).name(), "embedded");
    }

    #[test]
    fn asking_for_ripgrep_resolves_to_it_when_present_and_degrades_when_not() {
        let selected = Backend::select(Some("ripgrep"));
        match locate_ripgrep() {
            Some(_) => assert_eq!(selected.name(), "ripgrep"),
            None => assert_eq!(selected.name(), "embedded"),
        }
        assert_eq!(
            Backend::select(Some(" rg ")).name(),
            Backend::select(Some("ripgrep")).name(),
            "the alias and the canonical name resolve identically, whitespace included"
        );
    }

    #[test]
    fn an_explicit_path_is_taken_without_consulting_path() {
        let backend = Backend::ripgrep("/nowhere/rg");
        assert_eq!(backend.name(), "ripgrep");
        match backend {
            Backend::Ripgrep(engine) => {
                assert_eq!(engine.program(), std::path::Path::new("/nowhere/rg"));
            }
            Backend::Embedded(_) => panic!("an explicit path must select the ripgrep backend"),
        }
    }
}

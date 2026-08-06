use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oc_engine::terminal_lease::TerminalLease;
use oc_paths::ResolvedProject;
use url::Url;

const DEFAULT_MEMORY_LIMIT_MIB: usize = 512;
const DEFAULT_MAX_RESTARTS: usize = 3;

#[derive(Debug, Clone)]
pub struct JsHostPolicy {
    pub(crate) hook_timeout: Duration,
    pub(crate) memory_limit_mib: usize,
    pub(crate) max_restarts: usize,
}

impl Default for JsHostPolicy {
    fn default() -> Self {
        Self {
            hook_timeout: crate::DEFAULT_HOOK_TIMEOUT,
            memory_limit_mib: DEFAULT_MEMORY_LIMIT_MIB,
            max_restarts: DEFAULT_MAX_RESTARTS,
        }
    }
}

impl JsHostPolicy {
    #[must_use]
    pub const fn hook_timeout(mut self, timeout: Duration) -> Self {
        self.hook_timeout = timeout;
        self
    }

    #[must_use]
    pub const fn memory_limit_mib(mut self, limit: usize) -> Self {
        self.memory_limit_mib = limit;
        self
    }

    #[must_use]
    pub const fn max_restarts(mut self, max_restarts: usize) -> Self {
        self.max_restarts = max_restarts;
        self
    }
}

#[derive(Clone)]
pub struct JsHostConfig {
    pub(crate) project: ResolvedProject,
    pub(crate) directory: PathBuf,
    pub(crate) worktree: PathBuf,
    pub(crate) cache_dir: PathBuf,
    pub(crate) server_url: Url,
    pub(crate) terminal: Arc<dyn TerminalLease>,
    pub(crate) policy: JsHostPolicy,
    pub(crate) runtime_search_path: Option<OsString>,
}

impl JsHostConfig {
    #[must_use]
    pub fn new(
        project: ResolvedProject,
        server_url: Url,
        terminal: Arc<dyn TerminalLease>,
    ) -> Self {
        let directory = project.directory.clone();
        Self {
            project,
            directory: directory.clone(),
            worktree: directory,
            cache_dir: oc_paths::cache().join("js-plugins"),
            server_url,
            terminal,
            policy: JsHostPolicy::default(),
            runtime_search_path: None,
        }
    }

    #[must_use]
    pub fn directory(mut self, directory: impl AsRef<Path>) -> Self {
        self.directory = directory.as_ref().to_path_buf();
        self
    }

    #[must_use]
    pub fn worktree(mut self, worktree: impl AsRef<Path>) -> Self {
        self.worktree = worktree.as_ref().to_path_buf();
        self
    }

    #[must_use]
    pub fn cache_dir(mut self, cache_dir: impl AsRef<Path>) -> Self {
        self.cache_dir = cache_dir.as_ref().to_path_buf();
        self
    }

    #[must_use]
    pub fn policy(mut self, policy: JsHostPolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub fn runtime_search_path(mut self, path: OsString) -> Self {
        self.runtime_search_path = Some(path);
        self
    }
}

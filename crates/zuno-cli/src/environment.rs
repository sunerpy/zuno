//! Startup environment resolution without process-global mutation.
//!
//! Rust 2024 makes `std::env::set_var` unsafe and this workspace forbids unsafe
//! code. The CLI therefore resolves the environment as an [`zuno_paths::Env`] value
//! and starts its command process with those overrides. Downstream services see
//! the same real environment upstream's middleware writes, while unit tests can
//! inspect the value without racing another test.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use zuno_paths::Env;
use zuno_pty::BackgroundExecutionService;
use zuno_tools::exposure::ExposureFlags;

use crate::GlobalOptions;
use crate::cmd::child_turn::BackgroundJobSupervisor;

/// The non-`ZUNO_*` marker inherited by child agents.
pub const AGENT: &str = "AGENT";
/// Marks a process launched by Zuno.
pub const ZUNO: &str = "ZUNO";
/// Identifies the Zuno process to child integrations.
pub const ZUNO_PID: &str = "ZUNO_PID";
/// Enables the additional stderr log sink.
pub const ZUNO_PRINT_LOGS: &str = "ZUNO_PRINT_LOGS";
/// Selects one of the five supported log levels.
pub const ZUNO_LOG_LEVEL: &str = "ZUNO_LOG_LEVEL";

/// Environment values read by the CLI and its command implementations.
pub const ZUNO_FLAG_NAMES: [&str; 36] = [
    "ZUNO_ALWAYS_NOTIFY_UPDATE",
    "ZUNO_AUTO_HEAP_SNAPSHOT",
    "ZUNO_CLIENT",
    "ZUNO_CONFIG",
    "ZUNO_CONFIG_CONTENT",
    "ZUNO_CONFIG_DIR",
    "ZUNO_DB",
    "ZUNO_DISABLE_AUTOCOMPACT",
    "ZUNO_DISABLE_AUTOUPDATE",
    "ZUNO_DISABLE_CLAUDE_CODE",
    "ZUNO_DISABLE_FFF",
    "ZUNO_DISABLE_MODELS_FETCH",
    "ZUNO_DISABLE_MOUSE",
    "ZUNO_DISABLE_PROJECT_CONFIG",
    "ZUNO_DISABLE_PRUNE",
    "ZUNO_DISABLE_TERMINAL_TITLE",
    "ZUNO_EXPERIMENTAL",
    "ZUNO_EXPERIMENTAL_DISABLE_COPY_ON_SELECT",
    "ZUNO_EXPERIMENTAL_DISABLE_FILEWATCHER",
    "ZUNO_EXPERIMENTAL_FILEWATCHER",
    "ZUNO_EXPERIMENTAL_REFERENCES",
    "ZUNO_EXPERIMENTAL_WORKSPACES",
    "ZUNO_FAKE_VCS",
    "ZUNO_GIT_BASH_PATH",
    "ZUNO_MODELS_PATH",
    "ZUNO_MODELS_URL",
    "ZUNO_PERMISSION",
    "ZUNO_SERVER_PASSWORD",
    "ZUNO_SERVER_USERNAME",
    "ZUNO_SHOW_TTFD",
    "ZUNO_TUI_CONFIG",
    "ZUNO_WORKSPACE_ID",
    ZUNO,
    ZUNO_LOG_LEVEL,
    ZUNO_PID,
    ZUNO_PRINT_LOGS,
];

/// The complete flag snapshot handed to command implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZunoFlags {
    values: BTreeMap<&'static str, Option<String>>,
    /// Existing typed exposure semantics, including the experimental fallback rule.
    pub exposure: ExposureFlags,
}

impl ZunoFlags {
    /// Reads every known name from one immutable environment value.
    #[must_use]
    pub fn read(env: &Env) -> Self {
        let values = ZUNO_FLAG_NAMES
            .into_iter()
            .map(|name| (name, env.value(name).map(str::to_owned)))
            .collect();
        let exposure = ExposureFlags::from_lookup(|name| env.value(name).map(str::to_owned));
        Self { values, exposure }
    }

    /// The raw value, preserving set-but-empty separately from absence.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(Option::as_deref)
    }

    /// Every known name and the value observed for it, in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, Option<&str>)> {
        self.values
            .iter()
            .map(|(name, value)| (*name, value.as_deref()))
    }

    /// Number of variables read, including absent variables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no names are represented. Always false for a valid snapshot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// The environment command handlers run under and the flags resolved from it.
#[derive(Debug, Clone)]
pub struct StartupEnvironment {
    resolved: Env,
    overrides: BTreeMap<&'static str, String>,
    extensions: std::sync::Arc<zuno_extension::ExtensionRegistry>,
    background_executions: Arc<Mutex<HashMap<PathBuf, Weak<BackgroundExecutionService>>>>,
    background_jobs: Arc<Mutex<HashMap<PathBuf, BackgroundJobSupervisor>>>,
    /// All supported `ZUNO_*` values after CLI precedence is applied.
    pub flags: ZunoFlags,
}

impl PartialEq for StartupEnvironment {
    fn eq(&self, other: &Self) -> bool {
        self.resolved == other.resolved
            && self.overrides == other.overrides
            && self.flags == other.flags
            && std::sync::Arc::ptr_eq(&self.extensions, &other.extensions)
            && Arc::ptr_eq(&self.background_executions, &other.background_executions)
            && Arc::ptr_eq(&self.background_jobs, &other.background_jobs)
    }
}

impl Eq for StartupEnvironment {}

impl StartupEnvironment {
    /// Applies CLI middleware precedence to a process snapshot.
    #[must_use]
    pub fn resolve(base: &Env, globals: &GlobalOptions) -> Self {
        let mut overrides = BTreeMap::from([
            (AGENT, "1".to_owned()),
            (ZUNO, "1".to_owned()),
            (ZUNO_PID, std::process::id().to_string()),
        ]);
        if globals.print_logs {
            overrides.insert(ZUNO_PRINT_LOGS, "1".to_owned());
        }
        if let Some(level) = globals.log_level {
            overrides.insert(ZUNO_LOG_LEVEL, level.as_str().to_owned());
        }
        let resolved = overrides.iter().fold(base.clone(), |env, (name, value)| {
            env.with(*name, value.clone())
        });
        let flags = ZunoFlags::read(&resolved);
        Self {
            resolved,
            overrides,
            extensions: std::sync::Arc::new(zuno_extension::ExtensionRegistry::new()),
            background_executions: Arc::new(Mutex::new(HashMap::new())),
            background_jobs: Arc::new(Mutex::new(HashMap::new())),
            flags,
        }
    }

    /// The complete environment value command implementations should consult.
    #[must_use]
    pub fn resolved(&self) -> &Env {
        &self.resolved
    }

    /// Only the values this CLI must overlay when starting its command process.
    pub fn overrides(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.overrides
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
    }

    /// Process-local extension definitions shared by every surface and child host.
    #[must_use]
    pub fn extensions(&self) -> &std::sync::Arc<zuno_extension::ExtensionRegistry> {
        &self.extensions
    }

    /// Process-owned background execution service shared by every session in one
    /// workspace.
    ///
    /// Session switches and child hosts clone [`StartupEnvironment`], so resolving
    /// the service here keeps already-running commands alive and observable instead
    /// of binding them to whichever [`crate::cmd::turn::TurnHost`] happened to
    /// launch them.
    pub fn background_executions(
        &self,
        directory: &Path,
    ) -> Result<Arc<BackgroundExecutionService>, zuno_pty::BackgroundExecutionError> {
        let key = directory.to_path_buf();
        let mut services = self
            .background_executions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(service) = services.get(&key).and_then(Weak::upgrade) {
            return Ok(service);
        }
        let service = Arc::new(BackgroundExecutionService::open(
            directory
                .join(zuno_paths::PROJECT_DIRECTORY)
                .join("background"),
        )?);
        services.insert(key, Arc::downgrade(&service));
        Ok(service)
    }

    /// Process owner for durable child and product-agent jobs in one workspace.
    ///
    /// Unlike the weak background-terminal cache, this map keeps a strong owner:
    /// dropping a session host or remounting the TUI must not detach a Tokio task
    /// that can still commit durable job or inbox state.
    pub(crate) fn background_jobs(&self, directory: &Path) -> BackgroundJobSupervisor {
        let mut supervisors = self
            .background_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        supervisors
            .entry(directory.to_path_buf())
            .or_default()
            .clone()
    }

    /// Request cancellation for all process-owned delegated work.
    pub(crate) fn cancel_background_jobs(&self) {
        let supervisors = self
            .background_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for supervisor in supervisors {
            supervisor.cancel_all();
        }
    }

    /// Join all delegated work before the command's Tokio runtime is dropped.
    pub(crate) async fn wait_background_jobs(&self) {
        let supervisors = self
            .background_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for supervisor in supervisors {
            supervisor.wait_all().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CliLogLevel;

    #[test]
    fn snapshot_reads_every_known_zuno_name_even_when_absent() {
        let snapshot = ZunoFlags::read(&Env::empty());
        assert_eq!(snapshot.len(), ZUNO_FLAG_NAMES.len());
        assert_eq!(snapshot.iter().count(), ZUNO_FLAG_NAMES.len());
        assert!(!snapshot.is_empty());
        assert!(snapshot.iter().all(|(_, value)| value.is_none()));
    }

    #[test]
    fn globals_and_required_process_markers_are_applied_before_flags_are_read() {
        let globals = GlobalOptions {
            print_logs: true,
            log_level: Some(CliLogLevel::Warn),
        };
        let startup = StartupEnvironment::resolve(&Env::empty(), &globals);

        assert_eq!(startup.resolved().value(AGENT), Some("1"));
        assert_eq!(startup.flags.value(ZUNO), Some("1"));
        assert_eq!(
            startup.flags.value(ZUNO_PID),
            Some(std::process::id().to_string().as_str())
        );
        assert_eq!(startup.flags.value(ZUNO_PRINT_LOGS), Some("1"));
        assert_eq!(startup.flags.value(ZUNO_LOG_LEVEL), Some("WARN"));
    }

    #[test]
    fn unset_cli_options_preserve_existing_environment_values() {
        let base = Env::empty()
            .with(ZUNO_PRINT_LOGS, "1")
            .with(ZUNO_LOG_LEVEL, "DEBUG");
        let startup = StartupEnvironment::resolve(&base, &GlobalOptions::default());

        assert_eq!(startup.flags.value(ZUNO_PRINT_LOGS), Some("1"));
        assert_eq!(startup.flags.value(ZUNO_LOG_LEVEL), Some("DEBUG"));
    }

    #[test]
    fn specific_experimental_false_beats_blanket_true() {
        let env = Env::empty()
            .with("ZUNO_EXPERIMENTAL", "true")
            .with("ZUNO_EXPERIMENTAL_PLAN_MODE", "false");
        let snapshot = ZunoFlags::read(&env);
        assert!(!snapshot.exposure.experimental_plan_mode);
    }

    #[test]
    fn values_are_read_by_their_native_names() {
        let env = Env::empty()
            .with("ZUNO_EXPERIMENTAL", "true")
            .with("ZUNO_EXPERIMENTAL_PLAN_MODE", "false");
        let snapshot = ZunoFlags::read(&env);

        assert!(!snapshot.exposure.experimental_plan_mode);
    }

    #[test]
    fn clones_share_one_process_extension_registry_but_new_resolutions_do_not() {
        let first = StartupEnvironment::resolve(&Env::empty(), &GlobalOptions::default());
        let clone = first.clone();
        let restarted = StartupEnvironment::resolve(&Env::empty(), &GlobalOptions::default());

        assert!(std::sync::Arc::ptr_eq(
            first.extensions(),
            clone.extensions()
        ));
        assert!(!std::sync::Arc::ptr_eq(
            first.extensions(),
            restarted.extensions()
        ));
        assert!(Arc::ptr_eq(
            &first.background_executions,
            &clone.background_executions
        ));
        assert!(!Arc::ptr_eq(
            &first.background_executions,
            &restarted.background_executions
        ));
        assert!(Arc::ptr_eq(&first.background_jobs, &clone.background_jobs));
        assert!(!Arc::ptr_eq(
            &first.background_jobs,
            &restarted.background_jobs
        ));
    }

    #[tokio::test]
    async fn clones_share_workspace_background_jobs_but_new_processes_do_not() {
        let first = StartupEnvironment::resolve(&Env::empty(), &GlobalOptions::default());
        let clone = first.clone();
        let restarted = StartupEnvironment::resolve(&Env::empty(), &GlobalOptions::default());
        let directory = Path::new("/workspace");
        let jobs = first.background_jobs(directory);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancelled = cancellation.clone();
        jobs.spawn("job_test", "ses_test", cancellation, async move {
            cancelled.cancelled().await;
        });
        tokio::task::yield_now().await;

        assert!(
            clone
                .background_jobs(directory)
                .has_running_tasks("ses_test")
        );
        assert!(
            !restarted
                .background_jobs(directory)
                .has_running_tasks("ses_test")
        );

        clone.background_jobs(directory).cancel_all();
        clone.background_jobs(directory).wait_all().await;
        assert!(!jobs.has_running_tasks("ses_test"));
    }

    #[tokio::test]
    async fn clones_share_workspace_delegation_capacity_across_turn_hosts() {
        let first = StartupEnvironment::resolve(&Env::empty(), &GlobalOptions::default());
        let clone = first.clone();
        let restarted = StartupEnvironment::resolve(&Env::empty(), &GlobalOptions::default());
        let directory = Path::new("/workspace");
        let one = std::num::NonZeroUsize::new(1).expect("non-zero");

        let running = first
            .background_jobs(directory)
            .delegation_limiter(one)
            .acquire(&tokio_util::sync::CancellationToken::new())
            .await
            .expect("first turn host occupies the workspace slot");
        let independent = restarted
            .background_jobs(directory)
            .delegation_limiter(one)
            .acquire(&tokio_util::sync::CancellationToken::new())
            .await
            .expect("a new process owns independent capacity");

        let clone_limiter = clone.background_jobs(directory).delegation_limiter(one);
        let waiting = tokio::spawn(async move {
            clone_limiter
                .acquire(&tokio_util::sync::CancellationToken::new())
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "a replacement turn host bypassed the workspace-wide bound"
        );

        drop(running);
        let _replacement = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("released workspace capacity wakes the replacement host")
            .expect("replacement task survives")
            .expect("replacement turn host is admitted");
        drop(independent);
    }
}

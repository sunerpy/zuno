use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use futures::future::join_all;

use super::config::JsHostConfig;
use super::host::{JsHost, JsHostBuilder, JsHostLimits, JsPluginInput};
use super::plugin::JsPlugin;
use super::runtime::{JsRuntime, JsRuntimeKind, discover_runtime, discover_runtime_in};
use super::spec::{JsPluginSpec, PluginSource, SpecError, VersionGate};
use crate::PluginDiagnosticKind;

pub const JS_COMPAT_OPENCODE_VERSION: &str = "1.18.13";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedJsPlugin {
    pub package: &'static str,
    pub version: &'static str,
}

pub const SUPPORTED_JS_PLUGINS: [SupportedJsPlugin; 2] = [
    SupportedJsPlugin {
        package: "opencode-antigravity-auth",
        version: "1.6.0",
    },
    SupportedJsPlugin {
        package: "@sunerpy/opencode-kiro-auth",
        version: "0.20.1",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsDiagnosticKind {
    MissingRuntime,
    Install,
    MissingEntrypoint,
    Compatibility,
    FailedToLoad,
    Crashed,
    TimedOut,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsDiagnostic {
    pub plugin: String,
    pub kind: JsDiagnosticKind,
    pub message: String,
}

pub struct JsPluginLoad {
    plugins: Vec<Arc<JsPlugin>>,
    hosts: Vec<JsHost>,
    startup_diagnostics: Vec<JsDiagnostic>,
}

impl JsPluginLoad {
    #[must_use]
    pub fn plugins(&self) -> &[Arc<JsPlugin>] {
        &self.plugins
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<JsDiagnostic> {
        let mut diagnostics = self.startup_diagnostics.clone();
        for host in &self.hosts {
            diagnostics.extend(
                host.diagnostics()
                    .into_iter()
                    .map(|diagnostic| JsDiagnostic {
                        plugin: diagnostic.plugin,
                        kind: map_diagnostic_kind(diagnostic.kind),
                        message: diagnostic.message,
                    }),
            );
        }
        diagnostics
    }

    pub async fn shutdown(&self) {
        for host in &self.hosts {
            host.shutdown().await;
        }
    }
}

pub async fn load_js_plugins_ordered(
    specs: Vec<JsPluginSpec>,
    config: JsHostConfig,
) -> JsPluginLoad {
    let labels = specs
        .iter()
        .map(|spec| spec.spec().to_owned())
        .collect::<Vec<_>>();
    let discovered = match config.runtime_search_path.as_deref() {
        Some(path) => discover_runtime_in(Some(path), &labels),
        None => discover_runtime(&labels),
    };
    let runtime = match discovered {
        Ok(runtime) => runtime,
        Err(error) => {
            return JsPluginLoad {
                plugins: Vec::new(),
                hosts: Vec::new(),
                startup_diagnostics: labels
                    .into_iter()
                    .map(|plugin| JsDiagnostic {
                        plugin,
                        kind: JsDiagnosticKind::MissingRuntime,
                        message: error.to_string(),
                    })
                    .collect(),
            };
        }
    };
    let attempts = join_all(
        specs
            .into_iter()
            .map(|spec| load_one(spec, config.clone(), runtime.clone())),
    )
    .await;
    let mut plugins = Vec::new();
    let mut hosts = Vec::new();
    let mut startup_diagnostics = Vec::new();
    for attempt in attempts {
        match attempt {
            Ok((plugin, host, warning)) => {
                plugins.push(plugin);
                hosts.push(host);
                startup_diagnostics.extend(warning);
            }
            Err(diagnostic) => startup_diagnostics.push(diagnostic),
        }
    }
    JsPluginLoad {
        plugins,
        hosts,
        startup_diagnostics,
    }
}

async fn load_one(
    spec: JsPluginSpec,
    config: JsHostConfig,
    runtime: JsRuntime,
) -> Result<(Arc<JsPlugin>, JsHost, Vec<JsDiagnostic>), JsDiagnostic> {
    let label = spec.spec().to_owned();
    let resolved = resolve_or_install(&spec, &config.cache_dir, &runtime)
        .map_err(|error| spec_diagnostic(&label, error))?;
    let input = JsPluginInput {
        project: serde_json::json!({
            "id": config.project.id,
            "worktree": config.worktree,
            "vcs": config.project.vcs.as_ref().map(|_| "git"),
        }),
        directory: config.directory.clone(),
        worktree: config.worktree.clone(),
        server_url: config.server_url.to_string(),
        options: spec.configured_options().cloned(),
        sdk_module: None,
        loopback_port: None,
    };
    let limits = JsHostLimits {
        memory_ceiling: (config.policy.memory_limit_mib as u64) * 1024 * 1024,
        hook_timeout: config.policy.hook_timeout,
        max_restarts: config.policy.max_restarts.try_into().unwrap_or(u32::MAX),
        ..JsHostLimits::default()
    };
    let host = JsHostBuilder::new(&label, runtime, &spec, resolved.entry(), input)
        .with_limits(limits)
        .with_terminal_lease(Arc::clone(&config.terminal))
        .start()
        .await
        .map_err(|error| JsDiagnostic {
            plugin: label.clone(),
            kind: map_diagnostic_kind(error.kind()),
            message: error.to_string(),
        })?;
    let plugin = JsPlugin::build(host.clone(), &label).map_err(|error| JsDiagnostic {
        plugin: label.clone(),
        kind: JsDiagnosticKind::Protocol,
        message: error.to_string(),
    })?;
    let warning = match resolved.gate() {
        VersionGate::Unsatisfied { range, reported } => vec![JsDiagnostic {
            plugin: label,
            kind: JsDiagnosticKind::Compatibility,
            message: format!(
                "plugin declares @opencode-ai/plugin {range}; host reports {reported}"
            ),
        }],
        _ => Vec::new(),
    };
    Ok((plugin, host, warning))
}

fn resolve_or_install(
    spec: &JsPluginSpec,
    cache: &Path,
    runtime: &JsRuntime,
) -> Result<super::spec::ResolvedJsPlugin, SpecError> {
    match spec.resolve(cache) {
        Err(SpecError::NotInstalled { .. }) if spec.source() == PluginSource::Npm => {
            install_package(spec, cache, runtime)?;
            spec.resolve(cache)
        }
        result => result,
    }
}

fn install_package(
    spec: &JsPluginSpec,
    cache: &Path,
    runtime: &JsRuntime,
) -> Result<(), SpecError> {
    let package = spec.package().ok_or_else(|| SpecError::UnpinnedVersion {
        spec: spec.spec().to_owned(),
    })?;
    let version = spec.version().ok_or_else(|| SpecError::UnpinnedVersion {
        spec: spec.spec().to_owned(),
    })?;
    let slot = cache.join("packages").join(format!("{package}@{version}"));
    fs::create_dir_all(&slot).map_err(|error| SpecError::Install {
        spec: spec.spec().to_owned(),
        detail: error.to_string(),
    })?;
    let status = match runtime.kind() {
        JsRuntimeKind::Bun => Command::new(runtime.program())
            .args(["add", "--exact", "--cwd"])
            .arg(&slot)
            .arg(spec.spec())
            .status(),
        JsRuntimeKind::Node => Command::new("npm")
            .args(["install", "--prefix"])
            .arg(&slot)
            .arg(spec.spec())
            .status(),
    }
    .map_err(|error| SpecError::Install {
        spec: spec.spec().to_owned(),
        detail: error.to_string(),
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(SpecError::Install {
            spec: spec.spec().to_owned(),
            detail: format!("package installer exited with {status}"),
        })
    }
}

fn spec_diagnostic(plugin: &str, error: SpecError) -> JsDiagnostic {
    let kind = match error {
        SpecError::NoEntry { .. } => JsDiagnosticKind::MissingEntrypoint,
        SpecError::Install { .. } => JsDiagnosticKind::Install,
        _ => JsDiagnosticKind::FailedToLoad,
    };
    JsDiagnostic {
        plugin: plugin.to_owned(),
        kind,
        message: error.to_string(),
    }
}

pub(crate) const fn map_diagnostic_kind(kind: PluginDiagnosticKind) -> JsDiagnosticKind {
    match kind {
        PluginDiagnosticKind::FailedToLoad => JsDiagnosticKind::FailedToLoad,
        PluginDiagnosticKind::Crashed => JsDiagnosticKind::Crashed,
        PluginDiagnosticKind::TimedOut => JsDiagnosticKind::TimedOut,
        PluginDiagnosticKind::Protocol => JsDiagnosticKind::Protocol,
    }
}

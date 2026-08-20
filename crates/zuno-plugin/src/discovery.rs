use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use url::Url;
use zuno_config::Config;
use zuno_config::schema::plugin::PluginSpec;

/// Whether a declaration belongs to global or project-local configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginScope {
    Global,
    Local,
}

/// One parsed config layer with its declaring-file provenance.
#[derive(Debug, Clone, Copy)]
pub struct ConfigLayer<'a> {
    pub source: &'a Path,
    pub scope: PluginScope,
    pub config: &'a Config,
}

impl<'a> ConfigLayer<'a> {
    #[must_use]
    pub const fn new(source: &'a Path, scope: PluginScope, config: &'a Config) -> Self {
        Self {
            source,
            scope,
            config,
        }
    }
}

/// One directory searched for `{plugin,plugins}/*.{ts,js}`.
#[derive(Debug, Clone, Copy)]
pub struct ConfigDirectory<'a> {
    pub path: &'a Path,
    pub scope: PluginScope,
}

impl<'a> ConfigDirectory<'a> {
    #[must_use]
    pub const fn new(path: &'a Path, scope: PluginScope) -> Self {
        Self { path, scope }
    }
}

/// Provenance retained after plugin specs are merged and resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOrigin {
    Config { source: PathBuf, scope: PluginScope },
    AutoDiscovered { source: PathBuf, scope: PluginScope },
}

/// A loadable plugin spec plus the source that contributed it.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredPlugin {
    pub spec: PluginSpec,
    pub origin: PluginOrigin,
}

/// One executable found in a scanned directory, ready for the process tier.
///
/// Carries a `name` distinct from `program` because the process tier reports
/// failures against the entry a user can find on disk. A plugin that dies before
/// `plugin.initialize` never returns a manifest id, and "plugin `` failed" is not
/// something anybody can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredProcessPlugin {
    pub name: String,
    pub program: PathBuf,
    pub scope: PluginScope,
}

/// Failure while resolving or scanning plugin declarations.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("plugin config source `{path}` is not an absolute path")]
    RelativeConfigSource { path: PathBuf },
    #[error("plugin path `{path}` cannot be represented as a file URL")]
    FileUrl { path: PathBuf },
    #[error("failed to scan plugin directory `{path}`")]
    Scan {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Resolve configured specs and scan both auto-plugin directories in input order.
///
/// # Errors
/// Returns [`DiscoveryError`] when a local path has no absolute declaring base or
/// an existing auto-plugin directory cannot be read.
pub fn discover_plugins(
    layers: &[ConfigLayer<'_>],
    directories: &[ConfigDirectory<'_>],
) -> Result<Vec<DiscoveredPlugin>, DiscoveryError> {
    let configured = layers.iter().flat_map(|layer| {
        layer
            .config
            .plugin
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(move |spec| (layer, spec))
    });
    let mut plugins = Vec::new();
    for (layer, spec) in configured {
        plugins.push(DiscoveredPlugin {
            spec: resolve_plugin_spec(spec, layer.source)?,
            origin: PluginOrigin::Config {
                source: layer.source.to_path_buf(),
                scope: layer.scope,
            },
        });
    }
    for directory in directories {
        for child in ["plugin", "plugins"] {
            let path = directory.path.join(child);
            for source in scan_directory(&path)? {
                let spec = PluginSpec::Name(path_to_file_url(&source)?);
                plugins.push(DiscoveredPlugin {
                    spec,
                    origin: PluginOrigin::AutoDiscovered {
                        source,
                        scope: directory.scope,
                    },
                });
            }
        }
    }
    Ok(plugins)
}

/// Scan both auto-plugin directories for executables the process tier can spawn.
///
/// Mirrors [`discover_plugins`]'s shape — same two child directories, single level,
/// sorted by filename, symlinks accepted — and differs only in what counts as a
/// candidate. `.js` and `.ts` are excluded rather than merely unmatched: they are
/// the JavaScript tier's, and a file that is both a script and executable must not
/// be started twice.
///
/// # Errors
/// Returns [`DiscoveryError::Scan`] when an existing directory cannot be read.
pub fn discover_process_plugins(
    directories: &[ConfigDirectory<'_>],
) -> Result<Vec<DiscoveredProcessPlugin>, DiscoveryError> {
    let mut plugins = Vec::new();
    for directory in directories {
        for child in ["plugin", "plugins"] {
            let path = directory.path.join(child);
            for program in scan_executables(&path)? {
                let Some(name) = program.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                plugins.push(DiscoveredProcessPlugin {
                    name: name.to_owned(),
                    program,
                    scope: directory.scope,
                });
            }
        }
    }
    Ok(plugins)
}

fn scan_executables(path: &Path) -> Result<Vec<PathBuf>, DiscoveryError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(DiscoveryError::Scan {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| DiscoveryError::Scan {
            path: path.to_path_buf(),
            source,
        })?;
        let file = entry.path();
        if is_script(&file) {
            continue;
        }
        // `fs::metadata` and not `entry.metadata()`: the former follows symlinks, and a
        // symlink into a build directory is how a plugin author iterates. A broken link
        // resolves to nothing executable, so it is not a candidate either way.
        let Ok(metadata) = fs::metadata(&file) else {
            continue;
        };
        if metadata.is_file() && is_executable(&file, &metadata) {
            files.push(file);
        }
    }
    files.sort();
    Ok(files)
}

/// The executable bit is the signal: it is what `PATH` lookup itself uses.
///
/// No manifest and no extension convention, because requiring either would mean a
/// plugin author in a language with no build step could not ship one file.
#[cfg(unix)]
fn is_executable(_path: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o111 != 0
}

/// Windows has no executable bit, so the extension carries the same meaning.
///
/// This is the `PATHEXT` set minus the script hosts: a `.ps1` needs an interpreter
/// argument that `Command::new` cannot infer, so it is left out rather than spawned
/// in a way that would fail at run time.
#[cfg(windows)]
fn is_executable(path: &Path, _metadata: &fs::Metadata) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "exe" | "com" | "bat" | "cmd"
            )
        })
}

fn resolve_plugin_spec(
    spec: &PluginSpec,
    config_source: &Path,
) -> Result<PluginSpec, DiscoveryError> {
    let name = spec.name();
    if !is_path_spec(name) || name.starts_with("file://") {
        return Ok(spec.clone());
    }
    let path = Path::new(name);
    let resolved = if path.is_absolute() || is_windows_absolute(name) {
        path.to_path_buf()
    } else {
        let Some(parent) = config_source.parent() else {
            return Err(DiscoveryError::RelativeConfigSource {
                path: config_source.to_path_buf(),
            });
        };
        if !config_source.is_absolute() {
            return Err(DiscoveryError::RelativeConfigSource {
                path: config_source.to_path_buf(),
            });
        }
        parent.join(path)
    };
    let url = path_to_file_url(&resolved)?;
    Ok(match spec {
        PluginSpec::Name(_) => PluginSpec::Name(url),
        PluginSpec::WithOptions(_, options) => PluginSpec::WithOptions(url, options.clone()),
    })
}

fn is_path_spec(spec: &str) -> bool {
    spec.starts_with("file://")
        || spec.starts_with('.')
        || Path::new(spec).is_absolute()
        || is_windows_absolute(spec)
}

fn is_windows_absolute(spec: &str) -> bool {
    let bytes = spec.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn path_to_file_url(path: &Path) -> Result<String, DiscoveryError> {
    Url::from_file_path(path)
        .map(Url::into)
        .map_err(|()| DiscoveryError::FileUrl {
            path: path.to_path_buf(),
        })
}

fn scan_directory(path: &Path) -> Result<Vec<PathBuf>, DiscoveryError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(DiscoveryError::Scan {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| DiscoveryError::Scan {
            path: path.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| DiscoveryError::Scan {
            path: entry.path(),
            source,
        })?;
        let file = entry.path();
        if (file_type.is_file() || file_type.is_symlink()) && is_script(&file) {
            files.push(file);
        }
    }
    files.sort();
    Ok(files)
}

fn is_script(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "js" | "ts"))
}

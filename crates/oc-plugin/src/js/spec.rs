//! Turning a configured plugin spec into an entry point on disk.
//!
//! Three concerns that the oracle keeps together and this module keeps apart:
//!
//! 1. **Which kind of spec is it.** `file:` and path specs point at source the
//!    user controls; a bare name is an npm package that lives in the versioned
//!    cache. `packages/opencode/src/plugin/shared.ts:25` calls these
//!    `PluginSource`.
//! 2. **Where its entry point is.** `package.json`'s `exports` then `main`, then
//!    the conventional `index.js`. A package with no entry point is not an error
//!    to abort on — the oracle keeps its metadata so a `tui` package can still
//!    contribute a theme (`loader.ts:163-172`) — so the failure is reported and
//!    the rest of the plugins still load.
//! 3. **Whether its declared compatibility admits this host.** The version gate
//!    compares the package's `@opencode-ai/plugin` requirement against the version
//!    this binary reports, `1.18.13`. `file:` specs skip the gate: the user is
//!    editing that source right now and a range they never wrote must not stop
//!    them.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// The version this binary reports, and therefore the version the gate compares.
///
/// Kept as a literal string rather than derived from `CARGO_PKG_VERSION` because
/// the number is a *compatibility claim about the JavaScript API*, matching todo
/// 55's `--version`, not this crate's own release number.
pub const REPORTED_PLUGIN_API_VERSION: &str = "1.18.13";

/// The package whose version range gates a plugin.
pub const PLUGIN_PEER_PACKAGE: &str = "@opencode-ai/plugin";

/// Which entry point of a dual-entrypoint package to load.
///
/// `packages/opencode/src/plugin/shared.ts:26`. A package may default-export
/// `{ server }` or `{ tui }` but never both, and asking for the wrong one is an
/// error the shim raises with the oracle's own wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PluginKind {
    /// The server-side plugin: hooks, auth, providers, tools.
    #[default]
    Server,
    /// The TUI-side plugin.
    Tui,
}

impl PluginKind {
    /// The literal the shim and the oracle both use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Tui => "tui",
        }
    }
}

/// Where a spec's code comes from — `shared.ts:25`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    /// A `file:` URL or a filesystem path. Skips the version gate.
    File,
    /// A published package resolved out of the versioned cache.
    Npm,
}

/// A configured spec classified, plus the pieces resolution needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsPluginSpec {
    spec: String,
    source: PluginSource,
    kind: PluginKind,
    package: Option<String>,
    version: Option<String>,
    options: Option<Value>,
}

impl JsPluginSpec {
    /// Classify one configured specifier.
    ///
    /// Accepts the shapes `discover_plugins` produces: a `file://` URL, an
    /// absolute path, or `name`/`name@version`/`@scope/name@version`.
    #[must_use]
    pub fn parse(spec: &str) -> Self {
        let source = if spec.starts_with("file:")
            || spec.starts_with('.')
            || Path::new(spec).is_absolute()
        {
            PluginSource::File
        } else {
            PluginSource::Npm
        };
        let (package, version) = match source {
            PluginSource::File => (None, None),
            PluginSource::Npm => split_package_version(spec),
        };
        Self {
            spec: spec.to_owned(),
            source,
            kind: PluginKind::Server,
            package,
            version,
            options: None,
        }
    }

    /// Classify one configured specifier.
    #[must_use]
    pub fn new(spec: impl AsRef<str>) -> Self {
        Self::parse(spec.as_ref())
    }

    /// Attach options passed as the plugin factory's second argument.
    #[must_use]
    pub fn options(mut self, options: Value) -> Self {
        self.options = Some(options);
        self
    }

    /// Load the `tui` entry point instead of `server`.
    #[must_use]
    pub const fn with_kind(mut self, kind: PluginKind) -> Self {
        self.kind = kind;
        self
    }

    /// The specifier exactly as configured; appears in every diagnostic.
    #[must_use]
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// Whether this is a `file:` spec or an npm package.
    #[must_use]
    pub const fn source(&self) -> PluginSource {
        self.source
    }

    /// Which entry point to load.
    #[must_use]
    pub const fn kind(&self) -> PluginKind {
        self.kind
    }

    /// The npm package name, for an npm spec.
    #[must_use]
    pub fn package(&self) -> Option<&str> {
        self.package.as_deref()
    }

    /// The requested version, for an npm spec that pinned one.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Options passed as the plugin factory's second argument.
    #[must_use]
    pub fn configured_options(&self) -> Option<&Value> {
        self.options.as_ref()
    }

    /// The directory this spec's package occupies inside `cache`.
    ///
    /// Mirrors the layout opencode already writes, measured on this machine:
    /// `<cache>/packages/<pkg>@<version>/node_modules/<pkg>/`. Reproducing it
    /// rather than inventing one means an install performed by the real binary is
    /// reusable and an install performed here is too.
    #[must_use]
    pub fn cache_directory(&self, cache: &Path) -> Option<PathBuf> {
        let package = self.package.as_deref()?;
        let version = self.version.as_deref()?;
        Some(
            cache
                .join("packages")
                .join(format!("{package}@{version}"))
                .join("node_modules")
                .join(package),
        )
    }

    /// The absolute path to load, or the reason it cannot be determined.
    ///
    /// # Errors
    /// Returns [`SpecError`] when a `file:` URL is malformed, an npm spec has no
    /// pinned version, or the located package declares no usable entry point.
    pub fn resolve(&self, cache: &Path) -> Result<ResolvedJsPlugin, SpecError> {
        match self.source {
            PluginSource::File => {
                let path = file_spec_path(&self.spec)?;
                let entry = if path.is_dir() {
                    let manifest = read_manifest(&path.join("package.json"));
                    entry_point(&path, manifest.as_ref()).ok_or_else(|| SpecError::NoEntry {
                        spec: self.spec.clone(),
                        directory: path.clone(),
                    })?
                } else {
                    path
                };
                Ok(ResolvedJsPlugin {
                    spec: self.clone(),
                    entry,
                    manifest: None,
                    gate: VersionGate::Skipped,
                })
            }
            PluginSource::Npm => {
                let directory =
                    self.cache_directory(cache)
                        .ok_or_else(|| SpecError::UnpinnedVersion {
                            spec: self.spec.clone(),
                        })?;
                if !directory.is_dir() {
                    return Err(SpecError::NotInstalled {
                        spec: self.spec.clone(),
                        directory,
                    });
                }
                let manifest = read_manifest(&directory.join("package.json"));
                let entry = entry_point(&directory, manifest.as_ref()).ok_or_else(|| {
                    SpecError::NoEntry {
                        spec: self.spec.clone(),
                        directory: directory.clone(),
                    }
                })?;
                let gate = manifest
                    .as_ref()
                    .map_or(VersionGate::Undeclared, |manifest| {
                        gate_for(manifest, REPORTED_PLUGIN_API_VERSION)
                    });
                Ok(ResolvedJsPlugin {
                    spec: self.clone(),
                    entry,
                    manifest,
                    gate,
                })
            }
        }
    }
}

/// A spec resolved to a loadable entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedJsPlugin {
    spec: JsPluginSpec,
    entry: PathBuf,
    manifest: Option<PackageManifest>,
    gate: VersionGate,
}

impl ResolvedJsPlugin {
    /// The spec this came from.
    #[must_use]
    pub const fn spec(&self) -> &JsPluginSpec {
        &self.spec
    }

    /// The absolute module path the shim imports.
    #[must_use]
    pub fn entry(&self) -> &Path {
        &self.entry
    }

    /// The version-gate verdict.
    #[must_use]
    pub const fn gate(&self) -> &VersionGate {
        &self.gate
    }

    /// The package name and version pair this host is recorded as supporting.
    ///
    /// The plan asks for the supported package+version pairs to be *recorded*.
    /// This is that record, taken from the package's own manifest rather than from
    /// the spec string, so a cache directory that disagrees with its contents is
    /// visible.
    #[must_use]
    pub fn supported_pair(&self) -> Option<(String, String)> {
        let manifest = self.manifest.as_ref()?;
        Some((manifest.name.clone()?, manifest.version.clone()?))
    }

    /// Hooks the package advertises in `opencode.hooks`, if any.
    ///
    /// Advisory only. Kiro declares `auth, event, chat.headers` while its code
    /// registers `config, chat.headers, auth, provider`
    /// (`@sunerpy/opencode-kiro-auth@0.20.6/package.json:56-63` vs
    /// `dist/plugin.js:68,389,408,424`), so the manifest cannot be the dispatch
    /// authority — the loaded object is.
    #[must_use]
    pub fn declared_hooks(&self) -> &[String] {
        self.manifest
            .as_ref()
            .and_then(|manifest| manifest.opencode.as_ref())
            .map_or(&[], |section| section.hooks.as_deref().unwrap_or_default())
    }
}

/// The outcome of comparing a package's declared range against this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionGate {
    /// A `file:` spec. The user owns that source; no range is enforced.
    Skipped,
    /// The package names no `@opencode-ai/plugin` dependency.
    Undeclared,
    /// The declared range admits this host.
    Satisfied {
        /// The range as written in the package's manifest.
        range: String,
    },
    /// The declared range excludes this host. Load anyway, but say so.
    ///
    /// A refusal was rejected: every installed plugin here declares a range that
    /// predates `1.18.13` (`^0.15.30` and `^1.15.11`), so a hard gate would reject
    /// the two plugins this host exists to run. The verdict is therefore carried
    /// as a warning a caller can surface.
    Unsatisfied {
        /// The range as written.
        range: String,
        /// This host's reported version.
        reported: String,
    },
}

impl VersionGate {
    /// Whether the caller should warn about this plugin's declared range.
    #[must_use]
    pub const fn is_unsatisfied(&self) -> bool {
        matches!(self, Self::Unsatisfied { .. })
    }
}

/// A spec that cannot be turned into an entry point.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("plugin `{spec}` is not a valid file URL or path")]
    BadFileUrl { spec: String },
    #[error("plugin `{spec}` is an npm spec with no pinned version; expected `name@version`")]
    UnpinnedVersion { spec: String },
    #[error("plugin `{spec}` could not be installed: {detail}")]
    Install { spec: String, detail: String },
    #[error("plugin `{spec}` is not installed; expected it at `{}`", directory.display())]
    NotInstalled { spec: String, directory: PathBuf },
    #[error(
        "plugin `{spec}` declares no entry point; looked at package.json `exports`, \
         `main`, then index.{{js,mjs,cjs,ts}} under `{}`", directory.display()
    )]
    NoEntry { spec: String, directory: PathBuf },
}

/// The subset of `package.json` this host reads.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct PackageManifest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    main: Option<String>,
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    exports: Option<serde_json::Value>,
    #[serde(default)]
    dependencies: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    opencode: Option<OpencodeSection>,
}

/// The `opencode` block a plugin package may carry.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct OpencodeSection {
    #[serde(default)]
    hooks: Option<Vec<String>>,
}

fn read_manifest(path: &Path) -> Option<PackageManifest> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn entry_point(directory: &Path, manifest: Option<&PackageManifest>) -> Option<PathBuf> {
    if let Some(manifest) = manifest {
        for candidate in exports_candidates(manifest.exports.as_ref())
            .into_iter()
            .chain(manifest.module.clone())
            .chain(manifest.main.clone())
        {
            let path = directory.join(candidate.trim_start_matches("./"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    for name in ["index.js", "index.mjs", "index.cjs", "index.ts"] {
        let path = directory.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Pull string leaves out of an `exports` map, preferring `import` over `require`.
///
/// A full conditional-exports resolver is out of scope; every installed plugin
/// here uses plain `main`. The traversal exists so a package that only declares
/// `exports` still resolves rather than falling through to `index.js` by luck.
fn exports_candidates(exports: Option<&serde_json::Value>) -> Vec<String> {
    let mut found = Vec::new();
    fn walk(value: &serde_json::Value, found: &mut Vec<String>) {
        match value {
            serde_json::Value::String(path) => found.push(path.clone()),
            serde_json::Value::Object(map) => {
                for key in ["import", "module", "default", "require", "."] {
                    if let Some(child) = map.get(key) {
                        walk(child, found);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(exports) = exports {
        walk(exports, &mut found);
    }
    found
}

fn file_spec_path(spec: &str) -> Result<PathBuf, SpecError> {
    if let Some(rest) = spec.strip_prefix("file://") {
        if rest.is_empty() {
            return Err(SpecError::BadFileUrl {
                spec: spec.to_owned(),
            });
        }
        return Ok(PathBuf::from(rest));
    }
    if let Some(rest) = spec.strip_prefix("file:") {
        if rest.is_empty() {
            return Err(SpecError::BadFileUrl {
                spec: spec.to_owned(),
            });
        }
        return Ok(PathBuf::from(rest));
    }
    let path = PathBuf::from(spec);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(SpecError::BadFileUrl {
            spec: spec.to_owned(),
        })
    }
}

fn split_package_version(spec: &str) -> (Option<String>, Option<String>) {
    // `@scope/name@version`: the version separator is the last `@` that is not
    // the scope's leading one.
    let search_from = usize::from(spec.starts_with('@'));
    match spec[search_from..].rfind('@') {
        Some(offset) => {
            let at = search_from + offset;
            let (name, version) = spec.split_at(at);
            (Some(name.to_owned()), Some(version[1..].to_owned()))
        }
        None => (Some(spec.to_owned()), None),
    }
}

fn gate_for(manifest: &PackageManifest, reported: &str) -> VersionGate {
    let range = manifest
        .dependencies
        .as_ref()
        .and_then(|deps| deps.get(PLUGIN_PEER_PACKAGE))
        .or_else(|| {
            manifest
                .peer_dependencies
                .as_ref()
                .and_then(|deps| deps.get(PLUGIN_PEER_PACKAGE))
        });
    let Some(range) = range else {
        return VersionGate::Undeclared;
    };
    if range_admits(range, reported) {
        VersionGate::Satisfied {
            range: range.clone(),
        }
    } else {
        VersionGate::Unsatisfied {
            range: range.clone(),
            reported: reported.to_owned(),
        }
    }
}

/// A deliberately small npm-range check: `*`, `x`, exact, `^`, `~`, `>=`.
///
/// Full semver-range parsing is a dependency this crate does not have and does not
/// need: the gate's only job is to decide whether to attach a warning, and every
/// range observed in a real plugin manifest is one of these five shapes. An
/// unrecognised range is treated as admitting the host, because refusing to load
/// over an unparsed string would be the worse failure.
fn range_admits(range: &str, version: &str) -> bool {
    let range = range.trim();
    if range.is_empty() || range == "*" || range == "x" || range == "latest" {
        return true;
    }
    let Some(target) = Semver::parse(version) else {
        return true;
    };
    for clause in range.split("||") {
        if clause_admits(clause.trim(), target) {
            return true;
        }
    }
    false
}

fn clause_admits(clause: &str, target: Semver) -> bool {
    let (operator, rest) = match clause.as_bytes().first() {
        Some(b'^') => ("^", &clause[1..]),
        Some(b'~') => ("~", &clause[1..]),
        Some(b'>') if clause.starts_with(">=") => (">=", &clause[2..]),
        Some(b'>') => (">", &clause[1..]),
        Some(b'<') if clause.starts_with("<=") => ("<=", &clause[2..]),
        Some(b'<') => ("<", &clause[1..]),
        Some(b'=') => ("=", &clause[1..]),
        _ => ("=", clause),
    };
    let Some(bound) = Semver::parse(rest.trim()) else {
        return true;
    };
    match operator {
        "^" => {
            target >= bound
                && if bound.major == 0 {
                    target.major == 0 && target.minor == bound.minor
                } else {
                    target.major == bound.major
                }
        }
        "~" => target >= bound && target.major == bound.major && target.minor == bound.minor,
        ">=" => target >= bound,
        ">" => target > bound,
        "<=" => target <= bound,
        "<" => target < bound,
        _ => target == bound,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Semver {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Semver {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let core = text
            .split_once('-')
            .map_or(text, |(core, _)| core)
            .split_once('+')
            .map_or_else(
                || text.split_once('-').map_or(text, |(core, _)| core),
                |(core, _)| core,
            );
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let patch = parts.next().unwrap_or("0").parse().unwrap_or(0);
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

//! Built-in language-server definitions and configuration resolution.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zuno_catalog::lsp_config::ResolvedLsp;

/// How a missing built-in can be provisioned by an embedding application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    Npm(&'static str),
    Go(&'static str),
    Gem(&'static str),
    Dotnet(&'static str),
    Github(&'static str),
    Archive(&'static str),
}

/// A request passed to the injected installer. `zuno-lsp` never downloads itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub server_id: String,
    pub executable: String,
    pub kind: InstallKind,
}

/// Injectable seam for hosts that explicitly allow language-server downloads.
#[async_trait]
pub trait ServerInstaller: Send + Sync {
    async fn install(&self, request: &InstallRequest) -> Result<Option<PathBuf>, RegistryError>;
}

/// Default installer used by tests and offline deployments.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopInstaller;

#[async_trait]
impl ServerInstaller for NoopInstaller {
    async fn install(&self, _request: &InstallRequest) -> Result<Option<PathBuf>, RegistryError> {
        Ok(None)
    }
}

/// Registry and executable-resolution failures.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("language server {server_id} has an empty command")]
    EmptyCommand { server_id: String },
    #[error("language server {server_id} is not installed (looked for {executable})")]
    NotInstalled {
        server_id: String,
        executable: String,
    },
    #[error("installer failed for language server {server_id}")]
    Install {
        server_id: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Root-discovery policy associated with a server definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootPolicy {
    Nearest,
    Strict,
    Workspace,
    RustWorkspace,
    NixWorkspace,
}

/// Whether an enabled server can start, as [`ServerSpec::availability`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Its program resolves on `PATH` now.
    Present,
    /// Its program is absent but a built-in provisioning source would fetch it.
    Installable,
    /// Its program is absent and nothing would provide it.
    Missing,
    /// It has no command at all, which the schema should have refused.
    NoCommand,
}

/// One enabled server after built-ins and `lsp` overrides are merged.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSpec {
    pub id: String,
    pub extensions: Vec<String>,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub initialization: Value,
    pub root_policy: RootPolicy,
    pub root_markers: Vec<String>,
    pub exclude_markers: Vec<String>,
    pub install: Option<InstallKind>,
}

impl ServerSpec {
    /// Whether this server claims `file` by extension or exact filename.
    #[must_use]
    pub fn handles(&self, file: &Path) -> bool {
        let extension = file
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"));
        let filename = file.file_name().and_then(|value| value.to_str());
        self.extensions.iter().any(|candidate| {
            extension.as_deref() == Some(candidate.as_str()) || filename == Some(candidate.as_str())
        })
    }

    /// Whether this server could actually start, without starting it.
    ///
    /// Answers the question a status surface has to answer before any file is read, and
    /// answers it with the *same* `PATH` resolution [`ServerRegistry::launch_command`]
    /// will use — a second rule would let the panel promise a start the launch then
    /// refuses. It deliberately does not consult an installer, so it never provisions
    /// anything as a side effect of drawing a frame; a server that would be installed on
    /// demand reports [`Availability::Installable`] rather than being called present.
    #[must_use]
    pub fn availability(&self) -> Availability {
        let Some(executable) = self.command.first() else {
            return Availability::NoCommand;
        };
        if resolve_executable(executable).is_some() {
            Availability::Present
        } else if self.install.is_some() {
            Availability::Installable
        } else {
            Availability::Missing
        }
    }

    /// Resolve the project root without walking above `workspace`.
    #[must_use]
    pub fn root_for(&self, file: &Path, workspace: &Path) -> Option<PathBuf> {
        let start = if file.is_dir() { file } else { file.parent()? };
        if has_marker_between(start, workspace, &self.exclude_markers).is_some() {
            return None;
        }
        match self.root_policy {
            RootPolicy::Workspace => Some(workspace.to_path_buf()),
            RootPolicy::Nearest => has_marker_between(start, workspace, &self.root_markers)
                .or_else(|| Some(workspace.to_path_buf())),
            RootPolicy::Strict => has_marker_between(start, workspace, &self.root_markers),
            RootPolicy::NixWorkspace => {
                has_marker_between(start, workspace, &["flake.nix".to_owned()])
                    .or_else(|| Some(workspace.to_path_buf()))
            }
            RootPolicy::RustWorkspace => rust_workspace_root(start, workspace),
        }
    }
}

/// Enabled language servers in stable upstream declaration order.
pub struct ServerRegistry {
    servers: Vec<ServerSpec>,
    installer: Arc<dyn ServerInstaller>,
}

impl std::fmt::Debug for ServerRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerRegistry")
            .field("servers", &self.servers)
            .finish_non_exhaustive()
    }
}

impl ServerRegistry {
    /// Resolve built-ins and custom entries from the catalog's interpreted config.
    #[must_use]
    pub fn new(config: &ResolvedLsp, installer: Arc<dyn ServerInstaller>) -> Self {
        if !config.is_enabled() {
            return Self {
                servers: Vec::new(),
                installer,
            };
        }

        let mut servers = Vec::new();
        let mut seen = BTreeSet::new();
        for definition in BUILTINS {
            if !config.is_server_enabled(definition.id) {
                continue;
            }
            let override_entry = config.get(definition.id);
            servers.push(ServerSpec {
                id: definition.id.to_owned(),
                extensions: config
                    .extensions_for(definition.id)
                    .map_or_else(|| strings(definition.extensions), |items| items.to_vec()),
                command: config
                    .command_for(definition.id)
                    .map_or_else(|| strings(definition.command), |items| items.to_vec()),
                env: override_entry.map_or_else(BTreeMap::new, |entry| entry.env.clone()),
                initialization: config
                    .initialization_for(definition.id)
                    .cloned()
                    .map(Value::Object)
                    .unwrap_or_else(|| parse_initialization(definition.initialization)),
                root_policy: definition.root_policy,
                root_markers: strings(definition.root_markers),
                exclude_markers: strings(definition.exclude_markers),
                install: definition.install,
            });
            seen.insert(definition.id);
        }

        for custom in config.servers() {
            if seen.contains(custom.id.as_str()) {
                continue;
            }
            servers.push(ServerSpec {
                id: custom.id.clone(),
                extensions: custom.extensions.clone(),
                command: custom.command.clone(),
                env: custom.env.clone(),
                initialization: custom
                    .initialization
                    .clone()
                    .map(Value::Object)
                    .unwrap_or(Value::Null),
                root_policy: RootPolicy::Workspace,
                root_markers: Vec::new(),
                exclude_markers: Vec::new(),
                install: None,
            });
        }
        Self { servers, installer }
    }

    /// Offline registry with the default no-op installer.
    #[must_use]
    pub fn offline(config: &ResolvedLsp) -> Self {
        Self::new(config, Arc::new(NoopInstaller))
    }

    /// Enabled definitions in deterministic order.
    #[must_use]
    pub fn servers(&self) -> &[ServerSpec] {
        &self.servers
    }

    /// Enabled definitions that claim `file` and resolve a root.
    pub fn matching<'a>(
        &'a self,
        file: &'a Path,
        workspace: &'a Path,
    ) -> impl Iterator<Item = (&'a ServerSpec, PathBuf)> + 'a {
        self.servers.iter().filter_map(move |server| {
            server
                .handles(file)
                .then(|| server.root_for(file, workspace))
                .flatten()
                .map(|root| (server, root))
        })
    }

    /// Resolve argv[0] on `PATH`, invoking the configured installer only when a
    /// built-in declares a provisioning source.
    pub async fn launch_command(&self, server: &ServerSpec) -> Result<Vec<String>, RegistryError> {
        let Some(executable) = server.command.first() else {
            return Err(RegistryError::EmptyCommand {
                server_id: server.id.clone(),
            });
        };
        let resolved = resolve_executable(executable);
        let resolved = match (resolved, server.install) {
            (Some(path), _) => Some(path),
            (None, Some(kind)) => {
                self.installer
                    .install(&InstallRequest {
                        server_id: server.id.clone(),
                        executable: executable.clone(),
                        kind,
                    })
                    .await?
            }
            (None, None) => None,
        };
        let Some(resolved) = resolved else {
            return Err(RegistryError::NotInstalled {
                server_id: server.id.clone(),
                executable: executable.clone(),
            });
        };
        let mut command = server.command.clone();
        command[0] = resolved.to_string_lossy().into_owned();
        Ok(command)
    }
}

#[derive(Debug, Clone, Copy)]
struct Builtin {
    id: &'static str,
    extensions: &'static [&'static str],
    command: &'static [&'static str],
    root_policy: RootPolicy,
    root_markers: &'static [&'static str],
    exclude_markers: &'static [&'static str],
    initialization: &'static str,
    install: Option<InstallKind>,
}

macro_rules! builtin {
    ($id:literal, [$($ext:literal),*], [$($command:literal),+], $policy:ident, [$($root:literal),*], [$($exclude:literal),*], $initialization:literal, $install:expr) => {
        Builtin {
            id: $id,
            extensions: &[$($ext),*],
            command: &[$($command),+],
            root_policy: RootPolicy::$policy,
            root_markers: &[$($root),*],
            exclude_markers: &[$($exclude),*],
            initialization: $initialization,
            install: $install,
        }
    };
}

const BUILTINS: &[Builtin] = &[
    builtin!(
        "deno",
        [".ts", ".tsx", ".js", ".jsx", ".mjs"],
        ["deno", "lsp"],
        Strict,
        ["deno.json", "deno.jsonc"],
        [],
        "null",
        None
    ),
    builtin!(
        "typescript",
        [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"],
        ["typescript-language-server", "--stdio"],
        Nearest,
        [
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock"
        ],
        ["deno.json", "deno.jsonc"],
        "null",
        Some(InstallKind::Npm("typescript-language-server"))
    ),
    builtin!(
        "vue",
        [".vue"],
        ["vue-language-server", "--stdio"],
        Nearest,
        [
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock"
        ],
        [],
        "{}",
        Some(InstallKind::Npm("@vue/language-server"))
    ),
    builtin!(
        "eslint",
        [
            ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue"
        ],
        ["vscode-eslint-language-server", "--stdio"],
        Nearest,
        [
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock"
        ],
        [],
        "null",
        Some(InstallKind::Github("microsoft/vscode-eslint"))
    ),
    builtin!(
        "oxlint",
        [
            ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".astro",
            ".svelte"
        ],
        ["oxlint", "--lsp"],
        Nearest,
        [
            ".oxlintrc.json",
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock",
            "package.json"
        ],
        [],
        "null",
        None
    ),
    builtin!(
        "biome",
        [
            ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".json", ".jsonc",
            ".vue", ".astro", ".svelte", ".css", ".graphql", ".gql", ".html"
        ],
        ["biome", "lsp-proxy", "--stdio"],
        Nearest,
        [
            "biome.json",
            "biome.jsonc",
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock"
        ],
        [],
        "null",
        Some(InstallKind::Npm("@biomejs/biome"))
    ),
    builtin!(
        "gopls",
        [".go"],
        ["gopls"],
        Nearest,
        ["go.work", "go.mod", "go.sum"],
        [],
        "null",
        Some(InstallKind::Go("golang.org/x/tools/gopls@latest"))
    ),
    builtin!(
        "ruby-lsp",
        [".rb", ".rake", ".gemspec", ".ru"],
        ["rubocop", "--lsp"],
        Nearest,
        ["Gemfile"],
        [],
        "null",
        Some(InstallKind::Gem("rubocop"))
    ),
    builtin!(
        "ty",
        [".py", ".pyi"],
        ["ty", "server"],
        Nearest,
        [
            "pyproject.toml",
            "ty.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "pyrightconfig.json"
        ],
        [],
        "null",
        None
    ),
    builtin!(
        "pyright",
        [".py", ".pyi"],
        ["pyright-langserver", "--stdio"],
        Nearest,
        [
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "pyrightconfig.json"
        ],
        [],
        "null",
        Some(InstallKind::Npm("pyright"))
    ),
    builtin!(
        "elixir-ls",
        [".ex", ".exs"],
        ["elixir-ls"],
        Nearest,
        ["mix.exs", "mix.lock"],
        [],
        "null",
        Some(InstallKind::Github("elixir-lsp/elixir-ls"))
    ),
    builtin!(
        "zls",
        [".zig", ".zon"],
        ["zls"],
        Nearest,
        ["build.zig"],
        [],
        "null",
        Some(InstallKind::Github("zigtools/zls"))
    ),
    builtin!(
        "csharp",
        [".cs", ".csx"],
        ["roslyn-language-server", "--stdio", "--autoLoadProjects"],
        Nearest,
        ["*.slnx", "*.sln", "*.csproj", "global.json"],
        [],
        "null",
        Some(InstallKind::Dotnet("roslyn-language-server"))
    ),
    builtin!(
        "razor",
        [".razor", ".cshtml"],
        ["roslyn-language-server", "--stdio", "--autoLoadProjects"],
        Nearest,
        ["*.slnx", "*.sln", "*.csproj", "global.json"],
        [],
        "null",
        Some(InstallKind::Dotnet("roslyn-language-server"))
    ),
    builtin!(
        "fsharp",
        [".fs", ".fsi", ".fsx", ".fsscript"],
        ["fsautocomplete"],
        Nearest,
        ["*.slnx", "*.sln", "*.fsproj", "global.json"],
        [],
        "null",
        Some(InstallKind::Dotnet("fsautocomplete"))
    ),
    builtin!(
        "sourcekit-lsp",
        [".swift", ".objc", "objcpp"],
        ["sourcekit-lsp"],
        Nearest,
        ["Package.swift", "*.xcodeproj", "*.xcworkspace"],
        [],
        "null",
        None
    ),
    builtin!(
        "rust",
        [".rs"],
        ["rust-analyzer"],
        RustWorkspace,
        ["Cargo.toml", "Cargo.lock"],
        [],
        "null",
        None
    ),
    builtin!(
        "clangd",
        [
            ".c", ".cpp", ".cc", ".cxx", ".c++", ".h", ".hpp", ".hh", ".hxx", ".h++"
        ],
        ["clangd", "--background-index", "--clang-tidy"],
        Nearest,
        ["compile_commands.json", "compile_flags.txt", ".clangd"],
        [],
        "null",
        Some(InstallKind::Github("clangd/clangd"))
    ),
    builtin!(
        "svelte",
        [".svelte"],
        ["svelteserver", "--stdio"],
        Nearest,
        [
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock"
        ],
        [],
        "{}",
        Some(InstallKind::Npm("svelte-language-server"))
    ),
    builtin!(
        "astro",
        [".astro"],
        ["astro-ls", "--stdio"],
        Nearest,
        [
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock"
        ],
        [],
        "null",
        Some(InstallKind::Npm("@astrojs/language-server"))
    ),
    builtin!(
        "jdtls",
        [".java"],
        ["jdtls"],
        Strict,
        [
            "settings.gradle",
            "settings.gradle.kts",
            "gradlew",
            "gradlew.bat",
            "build.gradle",
            "build.gradle.kts",
            "pom.xml",
            ".project",
            ".classpath"
        ],
        [],
        "null",
        Some(InstallKind::Archive(
            "https://www.eclipse.org/downloads/download.php?file=/jdtls/snapshots/jdt-language-server-latest.tar.gz"
        ))
    ),
    builtin!(
        "kotlin-ls",
        [".kt", ".kts"],
        ["kotlin-lsp", "--stdio"],
        Nearest,
        [
            "settings.gradle.kts",
            "settings.gradle",
            "gradlew",
            "gradlew.bat",
            "build.gradle.kts",
            "build.gradle",
            "pom.xml"
        ],
        [],
        "null",
        Some(InstallKind::Github("Kotlin/kotlin-lsp"))
    ),
    builtin!(
        "yaml-ls",
        [".yaml", ".yml"],
        ["yaml-language-server", "--stdio"],
        Nearest,
        [
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock"
        ],
        [],
        "null",
        Some(InstallKind::Npm("yaml-language-server"))
    ),
    builtin!(
        "lua-ls",
        [".lua"],
        ["lua-language-server"],
        Nearest,
        [
            ".luarc.json",
            ".luarc.jsonc",
            ".luacheckrc",
            ".stylua.toml",
            "stylua.toml",
            "selene.toml",
            "selene.yml"
        ],
        [],
        "null",
        Some(InstallKind::Github("LuaLS/lua-language-server"))
    ),
    builtin!(
        "php intelephense",
        [".php"],
        ["intelephense", "--stdio"],
        Nearest,
        ["composer.json", "composer.lock", ".php-version"],
        [],
        "{\"telemetry\":{\"enabled\":false}}",
        Some(InstallKind::Npm("intelephense"))
    ),
    builtin!(
        "prisma",
        [".prisma"],
        ["prisma", "language-server"],
        Nearest,
        ["schema.prisma", "prisma"],
        ["package.json"],
        "null",
        None
    ),
    builtin!(
        "dart",
        [".dart"],
        ["dart", "language-server", "--lsp"],
        Nearest,
        ["pubspec.yaml", "analysis_options.yaml"],
        [],
        "null",
        None
    ),
    builtin!(
        "ocaml-lsp",
        [".ml", ".mli"],
        ["ocamllsp"],
        Nearest,
        ["dune-project", "dune-workspace", ".merlin", "opam"],
        [],
        "null",
        None
    ),
    builtin!(
        "bash",
        [".sh", ".bash", ".zsh", ".ksh"],
        ["bash-language-server", "start"],
        Workspace,
        [],
        [],
        "null",
        Some(InstallKind::Npm("bash-language-server"))
    ),
    builtin!(
        "terraform",
        [".tf", ".tfvars"],
        ["terraform-ls", "serve"],
        Nearest,
        [".terraform.lock.hcl", "terraform.tfstate", "*.tf"],
        [],
        "{\"experimentalFeatures\":{\"prefillRequiredFields\":true,\"validateOnSave\":true}}",
        Some(InstallKind::Github("hashicorp/terraform-ls"))
    ),
    builtin!(
        "texlab",
        [".tex", ".bib"],
        ["texlab"],
        Nearest,
        [".latexmkrc", "latexmkrc", ".texlabroot", "texlabroot"],
        [],
        "null",
        Some(InstallKind::Github("latex-lsp/texlab"))
    ),
    builtin!(
        "dockerfile",
        [".dockerfile", "Dockerfile"],
        ["docker-langserver", "--stdio"],
        Workspace,
        [],
        [],
        "null",
        Some(InstallKind::Npm("dockerfile-language-server-nodejs"))
    ),
    builtin!(
        "gleam",
        [".gleam"],
        ["gleam", "lsp"],
        Nearest,
        ["gleam.toml"],
        [],
        "null",
        None
    ),
    builtin!(
        "clojure-lsp",
        [".clj", ".cljs", ".cljc", ".edn"],
        ["clojure-lsp", "listen"],
        Nearest,
        [
            "deps.edn",
            "project.clj",
            "shadow-cljs.edn",
            "bb.edn",
            "build.boot"
        ],
        [],
        "null",
        None
    ),
    builtin!(
        "nixd",
        [".nix"],
        ["nixd"],
        NixWorkspace,
        ["flake.nix"],
        [],
        "null",
        None
    ),
    builtin!(
        "tinymist",
        [".typ", ".typc"],
        ["tinymist"],
        Nearest,
        ["typst.toml"],
        [],
        "null",
        Some(InstallKind::Github("Myriad-Dreamin/tinymist"))
    ),
    builtin!(
        "haskell-language-server",
        [".hs", ".lhs"],
        ["haskell-language-server-wrapper", "--lsp"],
        Nearest,
        ["stack.yaml", "cabal.project", "hie.yaml", "*.cabal"],
        [],
        "null",
        None
    ),
    builtin!(
        "julials",
        [".jl"],
        [
            "julia",
            "--startup-file=no",
            "--history-file=no",
            "-e",
            "using LanguageServer; runserver()"
        ],
        Nearest,
        ["Project.toml", "Manifest.toml", "*.jl"],
        [],
        "null",
        None
    ),
];

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn parse_initialization(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or(Value::Null)
}

fn resolve_executable(executable: &str) -> Option<PathBuf> {
    let path = Path::new(executable);
    if path.is_absolute() || executable.contains(std::path::MAIN_SEPARATOR) {
        return path.is_file().then(|| path.to_path_buf());
    }
    which::which(executable).ok()
}

fn has_marker_between(start: &Path, stop: &Path, markers: &[String]) -> Option<PathBuf> {
    if markers.is_empty() {
        return None;
    }
    let mut current = start.to_path_buf();
    loop {
        if directory_matches(&current, markers) {
            return Some(current);
        }
        if current == stop || !current.starts_with(stop) || !current.pop() {
            return None;
        }
    }
}

fn directory_matches(directory: &Path, markers: &[String]) -> bool {
    markers.iter().any(|marker| {
        if let Some(suffix) = marker.strip_prefix('*') {
            return std::fs::read_dir(directory).ok().is_some_and(|entries| {
                entries
                    .filter_map(Result::ok)
                    .any(|entry| entry.file_name().to_string_lossy().ends_with(suffix))
            });
        }
        directory.join(marker).exists()
    })
}

fn rust_workspace_root(start: &Path, workspace: &Path) -> Option<PathBuf> {
    let crate_root = has_marker_between(
        start,
        workspace,
        &["Cargo.toml".to_owned(), "Cargo.lock".to_owned()],
    )?;
    let mut current = crate_root.clone();
    let mut result = crate_root;
    loop {
        let manifest = current.join("Cargo.toml");
        if std::fs::read_to_string(manifest)
            .ok()
            .is_some_and(|content| content.contains("[workspace]"))
        {
            result.clone_from(&current);
            break;
        }
        if current == workspace || !current.starts_with(workspace) || !current.pop() {
            break;
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_catalog::lsp_config::{BUILTIN_SERVER_IDS, ResolvedLsp};
    use zuno_config::schema::lsp::LspConfig;

    #[test]
    fn table_covers_every_declared_builtin_once() {
        let ids: Vec<_> = BUILTINS.iter().map(|server| server.id).collect();
        assert_eq!(ids.len(), BUILTIN_SERVER_IDS.len());
        assert_eq!(ids, BUILTIN_SERVER_IDS);
        assert_eq!(
            ids.iter().copied().collect::<BTreeSet<_>>().len(),
            BUILTIN_SERVER_IDS.len()
        );
    }

    /// The census defect: a status surface built from `ResolvedLsp::servers()` — the
    /// *overrides* — reported `0 lsp` while `lsp: true` had every built-in enabled and the
    /// diagnostics probe was happily running them. The registry is the only honest count.
    #[test]
    fn enabling_lsp_wholesale_enables_every_builtin_not_zero_servers() {
        let overrides_only = ResolvedLsp::resolve(Some(&LspConfig::Enabled(true)));
        assert_eq!(
            overrides_only.servers().count(),
            0,
            "the trap: `lsp: true` declares no per-server override"
        );
        assert_eq!(
            ServerRegistry::offline(&overrides_only).servers().len(),
            BUILTIN_SERVER_IDS.len(),
            "every built-in is enabled, so a count of zero is a live feature reported absent"
        );
    }

    /// A truthful zero: with no `lsp` key nothing is enabled, so nothing will ever start.
    #[test]
    fn an_absent_lsp_key_really_does_enable_nothing() {
        let registry = ServerRegistry::offline(&ResolvedLsp::resolve(None));
        assert!(registry.servers().is_empty());
    }

    /// A status surface must be able to tell "will start when a file is read" from "can
    /// never start", and must not install anything to find out.
    #[test]
    fn availability_separates_a_present_program_from_a_missing_one() {
        let config: LspConfig = serde_json::from_value(serde_json::json!({
            "present": { "command": ["cargo"], "extensions": [".present"] },
            "absent": {
                "command": ["zuno-no-such-language-server"],
                "extensions": [".absent"]
            }
        }))
        .expect("valid LSP config");
        let registry = ServerRegistry::offline(&ResolvedLsp::resolve(Some(&config)));
        let availability = |id: &str| {
            registry
                .servers()
                .iter()
                .find(|server| server.id == id)
                .expect("the server stayed enabled")
                .availability()
        };
        assert_eq!(availability("present"), Availability::Present);
        assert_eq!(availability("absent"), Availability::Missing);
        // A built-in with a provisioning source is neither present nor hopeless, and
        // collapsing it into `Missing` would report an installable server as broken.
        let rust = registry
            .servers()
            .iter()
            .find(|server| server.id == "rust")
            .expect("rust is a built-in");
        assert!(
            matches!(
                rust.availability(),
                Availability::Present | Availability::Installable
            ),
            "a built-in that can be provisioned must never report Missing: {:?}",
            rust.availability()
        );
    }

    #[test]
    fn config_disables_one_server_and_overrides_another() {
        let config: LspConfig = serde_json::from_value(serde_json::json!({
            "rust": { "disabled": true },
            "typescript": {
                "command": ["my-ts", "--stdio"],
                "extensions": [".custom"],
                "env": { "MODE": "test" },
                "initialization": { "feature": true }
            },
            "my-lsp": {
                "command": ["my-lsp"],
                "extensions": [".mine"]
            }
        }))
        .expect("valid LSP config");
        let resolved = ResolvedLsp::resolve(Some(&config));
        let registry = ServerRegistry::offline(&resolved);
        assert!(registry.servers().iter().all(|server| server.id != "rust"));
        let typescript = registry
            .servers()
            .iter()
            .find(|server| server.id == "typescript")
            .expect("typescript remains enabled");
        assert_eq!(typescript.command, ["my-ts", "--stdio"]);
        assert_eq!(typescript.extensions, [".custom"]);
        assert_eq!(typescript.env.get("MODE").map(String::as_str), Some("test"));
        assert_eq!(typescript.initialization["feature"], true);
        assert_eq!(
            registry.servers().last().map(|server| server.id.as_str()),
            Some("my-lsp")
        );
    }

    #[test]
    fn roots_do_not_escape_the_workspace() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let workspace = temp.path().join("project");
        let source = workspace.join("crate/src/main.rs");
        std::fs::create_dir_all(source.parent().expect("source parent"))
            .expect("create source tree");
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate\"]\n",
        )
        .expect("workspace manifest");
        std::fs::write(
            workspace.join("crate/Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .expect("crate manifest");
        let config = LspConfig::Enabled(true);
        let registry = ServerRegistry::offline(&ResolvedLsp::resolve(Some(&config)));
        let rust = registry
            .servers()
            .iter()
            .find(|server| server.id == "rust")
            .expect("rust definition");
        assert_eq!(rust.root_for(&source, &workspace), Some(workspace));
    }
}

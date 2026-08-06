//! The built-in formatter table, ported as data.
//!
//! Oracle: `packages/opencode/src/format/formatter.ts` — 26 exported `Info`
//! constants, each `{ name, environment?, extensions, enabled(context) }`. There
//! the availability test is a **closure** per formatter; here it is an
//! [`Availability`] value, because a table of data can be walked by a test and a
//! table of closures cannot. [`DEFINITIONS`] therefore states the same facts in a
//! shape that makes "every built-in has a command and claims at least one
//! extension" a checkable property rather than a hope.
//!
//! `$FILE` is the oracle's placeholder (`format/index.ts:79`) and is substituted
//! at run time, never at construction — a definition is `'static` and the path is
//! not.
//!
//! # Where this diverges, and why
//!
//! The four node-hosted formatters (prettier, oxfmt, biome, and — via its vendor
//! directory — pint) resolve their binary through `Npm.which`, which reads a
//! *global* per-package install directory and **installs the package if it is
//! missing** (`packages/core/src/npm.ts:192-241`). This port will not install
//! anything to satisfy a format, so [`Availability::NodePackage`] and
//! [`Availability::NodeMarker`] resolve `node_modules/.bin/<bin>` walking up from
//! the edited file instead. A project that has the package installed formats; one
//! that does not is skipped rather than silently mutated by a download. The
//! declared dependency check itself is ported exactly.

/// Environment overrides, as `(key, value)` pairs.
///
/// A slice rather than a map so the whole definition stays a `const`; there are
/// never more than a handful and lookup is not on any hot path.
pub type Environment = &'static [(&'static str, &'static str)];

/// The oracle's `BUN_BE_BUN=1`, set for every node-hosted formatter
/// (`format/formatter.ts:40-42,89-91,112-114`).
pub const BUN_BE_BUN: Environment = &[("BUN_BE_BUN", "1")];

/// How a built-in decides it is available on this machine.
///
/// Each variant is one of the shapes `format/formatter.ts` actually uses. There
/// is deliberately no catch-all: a new upstream shape should force a new variant
/// and a decision, not be absorbed by a predicate that happens to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// `which(program)` and nothing else — the majority of the table.
    Program,

    /// `which(program)`, plus any of these files found walking up from the edited
    /// file's directory to the worktree (`Filesystem.findUp`).
    ProgramWithMarker(&'static [&'static str]),

    /// `which(program)`, plus a `--help` whose **first line** contains every one
    /// of these fragments. `air` uses this to distinguish the R formatter from
    /// unrelated binaries called `air` (`format/formatter.ts:218-234`).
    ProgramWithHelpFirstLine(&'static [&'static str]),

    /// `which(program)`, plus a `--help` that merely has to exit zero — `uv`
    /// probes for subcommand support rather than identity
    /// (`format/formatter.ts:236-247`).
    ProgramWithHelpExitZero,

    /// Any of these marker files found walking up, with the binary resolved
    /// through the node package manager rather than `PATH`.
    NodeMarker(&'static [&'static str]),

    /// A JSON manifest found walking up must declare `package` under one of
    /// `keys`; the binary is then resolved through the node package manager.
    NodePackage {
        /// The manifest to look for, e.g. `package.json`.
        manifest: &'static str,
        /// The dependency maps to look in, in order.
        keys: &'static [&'static str],
        /// The package name that has to appear.
        package: &'static str,
    },

    /// A JSON manifest must declare `package`, and the command is the vendored
    /// path already written into [`Definition::command`] — `pint` runs
    /// `./vendor/bin/pint`, never a `PATH` lookup
    /// (`format/formatter.ts:360-374`).
    VendoredPackage {
        /// The manifest to look for, e.g. `composer.json`.
        manifest: &'static str,
        /// The dependency maps to look in, in order.
        keys: &'static [&'static str],
        /// The package name that has to appear.
        package: &'static str,
    },

    /// Ruff's layered check (`format/formatter.ts:189-216`): the binary, then
    /// either a ruff config file — where `pyproject.toml` additionally has to
    /// contain `[tool.ruff]` — or a dependency manifest mentioning `ruff`.
    RuffConfig,
}

/// One built-in formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Definition {
    /// The name the config addresses this formatter by. Note two of these differ
    /// from their `export const`: `clang` exports as `"clang-format"` and
    /// `rlang` as `"air"` — it is the `name` field the config keys on.
    pub name: &'static str,
    /// The command, argv-style, with `$FILE` where the path goes.
    pub command: &'static [&'static str],
    /// The extensions this formatter claims, leading dot included, because the
    /// oracle compares against `path.extname()` output (`format/index.ts:58`).
    pub extensions: &'static [&'static str],
    /// Extra environment for the command.
    pub environment: Environment,
    /// How this formatter decides it can run.
    pub availability: Availability,
    /// Only offered when the `experimentalOxfmt` runtime flag is set
    /// (`format/formatter.ts:96`).
    pub experimental: bool,
    /// A formatter that wins over this one when it is itself available. `uv`
    /// stands down for `ruff` because they are one backend
    /// (`format/formatter.ts:238`).
    pub shadowed_by: Option<&'static str>,
}

impl Definition {
    /// The program the command invokes, before `$FILE` substitution.
    ///
    /// Empty only for a malformed definition; the table test forbids that.
    #[must_use]
    pub fn program(&self) -> &'static str {
        self.command.first().copied().unwrap_or_default()
    }

    /// Whether this formatter claims `extension` (leading dot included).
    ///
    /// Case-sensitive, matching the oracle: `clang-format` claims both `.C` and
    /// `.c`, and `.H` and `.h`, as separate entries
    /// (`format/formatter.ts:168`), which only means anything under a
    /// case-sensitive comparison.
    #[must_use]
    pub fn claims(&self, extension: &str) -> bool {
        self.extensions.contains(&extension)
    }
}

/// Shorthand for the common case: a `PATH` lookup, no environment, not
/// experimental, nothing shadows it.
const fn program(
    name: &'static str,
    command: &'static [&'static str],
    extensions: &'static [&'static str],
) -> Definition {
    Definition {
        name,
        command,
        extensions,
        environment: &[],
        availability: Availability::Program,
        experimental: false,
        shadowed_by: None,
    }
}

/// The extension set prettier and biome share (`format/formatter.ts:43-67`
/// and `115-139` — the two lists are identical, 26 entries).
const WEB_EXTENSIONS: &[&str] = &[
    ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".html", ".htm", ".css", ".scss",
    ".sass", ".less", ".vue", ".svelte", ".json", ".jsonc", ".yaml", ".yml", ".toml", ".xml",
    ".md", ".mdx", ".graphql", ".gql",
];

/// The JavaScript and TypeScript subset oxfmt claims
/// (`format/formatter.ts:93`).
const SCRIPT_EXTENSIONS: &[&str] = &[".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts"];

/// The Ruby extensions rubocop and standardrb share
/// (`format/formatter.ts:251,261`).
const RUBY_EXTENSIONS: &[&str] = &[".rb", ".rake", ".gemspec", ".ru"];

/// The Python extensions ruff and uv share (`format/formatter.ts:191,238`).
const PYTHON_EXTENSIONS: &[&str] = &[".py", ".pyi"];

/// The `node_modules/.bin` dependency keys of a `package.json`.
const NPM_DEPENDENCY_KEYS: &[&str] = &["dependencies", "devDependencies"];

/// Every built-in formatter, in the oracle's declaration order.
///
/// Order is preserved because `format/index.ts:129-131` populates the registry by
/// iterating this module, and two formatters can claim one extension (prettier
/// and biome; rubocop and standardrb) — so which runs first is observable.
pub const DEFINITIONS: &[Definition] = &[
    program("gofmt", &["gofmt", "-w", "$FILE"], &[".go"]),
    program(
        "mix",
        &["mix", "format", "$FILE"],
        &[".ex", ".exs", ".eex", ".heex", ".leex", ".neex", ".sface"],
    ),
    Definition {
        name: "prettier",
        command: &["prettier", "--write", "$FILE"],
        extensions: WEB_EXTENSIONS,
        environment: BUN_BE_BUN,
        availability: Availability::NodePackage {
            manifest: "package.json",
            keys: NPM_DEPENDENCY_KEYS,
            package: "prettier",
        },
        experimental: false,
        shadowed_by: None,
    },
    Definition {
        name: "oxfmt",
        command: &["oxfmt", "$FILE"],
        extensions: SCRIPT_EXTENSIONS,
        environment: BUN_BE_BUN,
        availability: Availability::NodePackage {
            manifest: "package.json",
            keys: NPM_DEPENDENCY_KEYS,
            package: "oxfmt",
        },
        experimental: true,
        shadowed_by: None,
    },
    Definition {
        name: "biome",
        command: &["biome", "format", "--write", "$FILE"],
        extensions: WEB_EXTENSIONS,
        environment: BUN_BE_BUN,
        availability: Availability::NodeMarker(&["biome.json", "biome.jsonc"]),
        experimental: false,
        shadowed_by: None,
    },
    program("zig", &["zig", "fmt", "$FILE"], &[".zig", ".zon"]),
    Definition {
        name: "clang-format",
        command: &["clang-format", "-i", "$FILE"],
        extensions: &[
            ".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx", ".h++", ".ino", ".C",
            ".H",
        ],
        environment: &[],
        availability: Availability::ProgramWithMarker(&[".clang-format"]),
        experimental: false,
        shadowed_by: None,
    },
    program("ktlint", &["ktlint", "-F", "$FILE"], &[".kt", ".kts"]),
    Definition {
        name: "ruff",
        command: &["ruff", "format", "$FILE"],
        extensions: PYTHON_EXTENSIONS,
        environment: &[],
        availability: Availability::RuffConfig,
        experimental: false,
        shadowed_by: None,
    },
    Definition {
        name: "air",
        command: &["air", "format", "$FILE"],
        extensions: &[".R"],
        environment: &[],
        availability: Availability::ProgramWithHelpFirstLine(&["R language", "formatter"]),
        experimental: false,
        shadowed_by: None,
    },
    Definition {
        name: "uv",
        command: &["uv", "format", "--", "$FILE"],
        extensions: PYTHON_EXTENSIONS,
        environment: &[],
        availability: Availability::ProgramWithHelpExitZero,
        experimental: false,
        shadowed_by: Some("ruff"),
    },
    program(
        "rubocop",
        &["rubocop", "--autocorrect", "$FILE"],
        RUBY_EXTENSIONS,
    ),
    program(
        "standardrb",
        &["standardrb", "--fix", "$FILE"],
        RUBY_EXTENSIONS,
    ),
    program(
        "htmlbeautifier",
        &["htmlbeautifier", "$FILE"],
        &[".erb", ".html.erb"],
    ),
    program("dart", &["dart", "format", "$FILE"], &[".dart"]),
    Definition {
        name: "ocamlformat",
        command: &["ocamlformat", "-i", "$FILE"],
        extensions: &[".ml", ".mli"],
        environment: &[],
        availability: Availability::ProgramWithMarker(&[".ocamlformat"]),
        experimental: false,
        shadowed_by: None,
    },
    program(
        "terraform",
        &["terraform", "fmt", "$FILE"],
        &[".tf", ".tfvars"],
    ),
    program(
        "latexindent",
        &["latexindent", "-w", "-s", "$FILE"],
        &[".tex"],
    ),
    program("gleam", &["gleam", "format", "$FILE"], &[".gleam"]),
    program("shfmt", &["shfmt", "-w", "$FILE"], &[".sh", ".bash"]),
    program("nixfmt", &["nixfmt", "$FILE"], &[".nix"]),
    program("rustfmt", &["rustfmt", "$FILE"], &[".rs"]),
    Definition {
        name: "pint",
        command: &["./vendor/bin/pint", "$FILE"],
        extensions: &[".php"],
        environment: &[],
        availability: Availability::VendoredPackage {
            manifest: "composer.json",
            keys: &["require", "require-dev"],
            package: "laravel/pint",
        },
        experimental: false,
        shadowed_by: None,
    },
    program("ormolu", &["ormolu", "-i", "$FILE"], &[".hs"]),
    program(
        "cljfmt",
        &["cljfmt", "fix", "--quiet", "$FILE"],
        &[".clj", ".cljs", ".cljc", ".edn"],
    ),
    program("dfmt", &["dfmt", "-i", "$FILE"], &[".d"]),
];

/// The built-in named `name`, if there is one.
///
/// A config key that names no built-in is not an error — it declares a formatter
/// of its own, which is why `format/index.ts:150` tolerates `builtIn` being
/// `undefined`.
#[must_use]
pub fn definition(name: &str) -> Option<&'static Definition> {
    DEFINITIONS.iter().find(|entry| entry.name == name)
}

/// Every built-in that claims `extension`, in declaration order.
pub fn for_extension(extension: &str) -> impl Iterator<Item = &'static Definition> + '_ {
    DEFINITIONS
        .iter()
        .filter(move |entry| entry.claims(extension))
}

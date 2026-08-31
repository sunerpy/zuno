//! The release pipeline's invariants, asserted from inside `cargo test`.
//!
//! Everything here holds a property of the *shipped artifact* or of the pipeline
//! that produces it. All of it runs offline, so `make ci` enforces it on a machine
//! with no network — which is where this workspace's gates actually run.
//!
//! # Why these live next to `zuno-cli` and not in a shell script
//!
//! `zuno` is built from this crate, so "what is in the artifact" is a
//! question about this crate's dependency graph. Putting the answer in a
//! `#[test]` means `cargo test` alone enforces it, with no separate gate to
//! remember and no second CI step that can be dropped.
//!
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Crates that link OpenSSL, or select something that does.
///
/// `openssl-probe` is deliberately ABSENT. It only *locates* the host certificate
/// store, links nothing, and is legitimately in the graph through
/// `rustls-native-certs` <- `rustls-platform-verifier` <- `reqwest`. A check that
/// matched the substring `openssl` would flag it, and the flag would eventually
/// be "fixed" by loosening the check until it stopped catching the real thing.
const OPENSSL_CRATES: &[&str] = &["openssl", "openssl-sys", "openssl-src", "native-tls"];

/// The WebAssembly runtime intentionally shipped for constrained WASI plugins,
/// matched as crate *families*.
///
/// Unlike the OpenSSL list these are prefixes: Wasmtime resolves to many
/// packages (`wasmtime-cranelift`, `cranelift-codegen`, `wasmparser`,
/// `wasmtime-environ`, …). The release gate checks the two public host crates
/// exactly and uses this family matcher only to guard the graph inspection helper.
const WASM_RUNTIME_FAMILIES: &[&str] = &["wasmtime", "cranelift", "wasmparser", "wasm-encoder"];

/// Native dynamic-library loaders are not a supported plugin ABI.
///
/// Runtime-loadable Rust code uses the WASI Component Model or a contained process;
/// loading an arbitrary `.so`/`.dylib`/`.dll` would make unload safety and Rust ABI
/// compatibility unverifiable.
const DYNAMIC_PLUGIN_LOADER_CRATES: &[&str] =
    &["abi_stable", "dlopen", "dlopen2", "libffi", "libloading"];

/// Packages whose presence proves TLS is still *there*, in rustls form.
///
/// Without this the no-OpenSSL assertion could pass because TLS vanished
/// altogether, which is a check that can only detect one shape of failure.
const REQUIRED_TLS_CRATES: &[&str] = &["reqwest", "rustls", "rustls-webpki"];

/// The targets the release pipeline builds, smoke-tests, and publishes.
///
/// `aarch64-pc-windows-msvc` is deliberately absent. CodeBuild has no ARM Windows
/// compute in any region and an x86_64 Windows host cannot execute an ARM64
/// binary, so that target could only ever have been built and never run — the one
/// thing this pipeline exists to refuse. Its only other home was a GitHub-hosted
/// `windows-11-arm` runner, and the exhausted Actions minutes there are why the
/// release pipeline had never completed a single run. See release.yml's header.
const RELEASE_TARGETS: [&str; 5] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
];

/// The CodeBuild runner projects the workflows may route jobs to.
///
/// Three rather than one because CodeBuild takes the machine from the project and
/// a project has exactly one environment type: Linux, Windows, and macOS need one
/// each. Held as a list so [`codebuild_label_sets`] sees every routed job — a
/// helper that recognised only the Linux project would silently stop counting the
/// macOS and Windows legs, and the uniqueness invariant below would then be
/// asserted over a subset while reading as though it covered everything.
const RUNNER_PROJECTS: [&str; 3] = ["zuno-runner", "zuno-runner-macos", "zuno-runner-windows"];

/// The one cassette the artifact smoke test replays.
const SMOKE_CASSETTE: &str = "openai-chat/drives-a-tool-loop-end-to-end";

/// Floors, not exact counts. They exist so that a scan pointed at the wrong
/// directory — which a shared `CARGO_TARGET_DIR` across git worktrees really does
/// cause — fails loudly instead of passing vacuously.
///
/// `MINIMUM_CRATES` is deliberately *not* the roster gate. A floor cannot notice
/// an addition, which is exactly how `zuno-process` and `zuno-reaping-fixture` entered
/// the workspace unremarked; the exact roster is asserted against `crates.expected`
/// by [`the_workspace_roster_matches_the_declared_crate_list`].
const MINIMUM_CRATES: usize = 37;
const MINIMUM_SOURCE_FILES: usize = 300;
const MINIMUM_GRAPH_PACKAGES: usize = 100;

/// The declared roster, relative to the workspace root.
const ROSTER_FIXTURE: &str = "crates.expected";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR is always <root>/crates/zuno-cli")
        .to_path_buf()
}

/// Every package in a default, non-dev dependency graph, as `name version` lines.
///
/// `--locked --offline` and `CARGO_NET_OFFLINE` so this runs on a machine with no
/// network; `-e normal` so a dev-dependency cannot be mistaken for something that
/// ships; `--prefix none` so each line starts with the package name.
fn default_graph(selector: &[&str]) -> Vec<String> {
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .env("CARGO_NET_OFFLINE", "true")
        .args(["tree", "--locked", "--offline"])
        .args(selector)
        .args(["-e", "normal", "--prefix", "none"])
        .output()
        .expect("cargo tree must be runnable");
    assert!(
        output.status.success(),
        "cargo tree {selector:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout).expect("cargo tree output must be UTF-8");
    let mut packages: Vec<String> = tree
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

/// Whether `graph` contains a package with exactly this name.
///
/// Matches `"<name> "` rather than a substring, so `openssl` does not match
/// `openssl-probe` and `wasmtime` does not match a crate merely named after it.
fn contains_package(graph: &[String], name: &str) -> bool {
    let prefix = format!("{name} ");
    graph.iter().any(|line| line.starts_with(&prefix))
}

/// Packages whose name is exactly one of `names`.
fn exact_matches<'a>(graph: &'a [String], names: &[&str]) -> Vec<&'a String> {
    graph
        .iter()
        .filter(|line| {
            names
                .iter()
                .any(|name| line.starts_with(&format!("{name} ")))
        })
        .collect()
}

/// Packages whose name is one of `families` or begins with one followed by `-`.
fn family_matches<'a>(graph: &'a [String], families: &[&str]) -> Vec<&'a String> {
    graph
        .iter()
        .filter(|line| {
            families.iter().any(|family| {
                line.starts_with(&format!("{family} ")) || line.starts_with(&format!("{family}-"))
            })
        })
        .collect()
}

/// Why a package is in the graph, as `cargo tree -i` reports it.
///
/// Called only on failure, so a violation names the path that introduced the
/// dependency instead of leaving someone to go find it.
fn inverted_path(package: &str) -> String {
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .env("CARGO_NET_OFFLINE", "true")
        .args([
            "tree",
            "--locked",
            "--offline",
            "--workspace",
            "-e",
            "normal",
            "-i",
            package,
        ])
        .output();
    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(error) => format!("(could not run cargo tree -i {package}: {error})"),
    }
}

#[test]
fn the_shipped_binary_pulls_in_no_openssl() {
    let graph = default_graph(&["-p", "zuno-cli"]);
    assert!(
        graph.len() >= MINIMUM_GRAPH_PACKAGES,
        "the graph for zuno-cli has only {} packages; cargo tree returned something \
         unexpected and this assertion would pass vacuously",
        graph.len()
    );
    assert!(
        contains_package(&graph, "zuno-cli"),
        "cargo tree did not report zuno-cli itself; the graph is not the one intended"
    );

    let offenders = exact_matches(&graph, OPENSSL_CRATES);
    assert!(
        offenders.is_empty(),
        "the default zuno-cli graph contains OpenSSL: {offenders:?}\n\
         TLS in this workspace is rustls only (plan todo 1: reqwest carries \
         `default-features = false` and the `rustls` feature).\n\
         How each one got in:\n{}",
        offenders
            .iter()
            .map(|line| {
                let name = line.split_whitespace().next().unwrap_or(line);
                format!("--- cargo tree -i {name} ---\n{}", inverted_path(name))
            })
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The positive half. Without it this test would also pass if TLS disappeared,
    // and "no OpenSSL because no HTTPS" is not the property being claimed.
    for required in REQUIRED_TLS_CRATES {
        assert!(
            contains_package(&graph, required),
            "the default zuno-cli graph has no `{required}`; the no-OpenSSL result \
             above would then be meaningless, because there would be no TLS stack \
             at all"
        );
    }
}

/// A helper that renders how each offending package entered the graph, so a
/// violation names the dependency path instead of leaving someone to go find it.
fn explain(offenders: &[&String]) -> String {
    offenders
        .iter()
        .map(|line| {
            let name = line.split_whitespace().next().unwrap_or(line);
            format!("--- cargo tree -i {name} ---\n{}", inverted_path(name))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_shipped_binary_contains_the_wasi_component_host() {
    let graph = default_graph(&["-p", "zuno-cli"]);
    assert!(
        graph.len() >= MINIMUM_GRAPH_PACKAGES,
        "the graph for zuno-cli has only {} packages; this assertion would pass vacuously",
        graph.len()
    );
    for required in ["wasmtime", "wasmtime-wasi"] {
        assert!(
            contains_package(&graph, required),
            "the shipped binary has no `{required}`; runtime-loadable WASI component \
             plugins would be advertised without an executable host"
        );
    }
    assert!(
        !family_matches(&graph, WASM_RUNTIME_FAMILIES).is_empty(),
        "the WASI host crates are present but the family matcher saw no runtime packages"
    );
}

#[test]
fn the_shipped_binary_has_no_legacy_plugin_runtime() {
    let graph = default_graph(&["-p", "zuno-cli"]);
    assert!(
        graph.len() >= MINIMUM_GRAPH_PACKAGES,
        "the graph for zuno-cli has only {} packages; this assertion would pass vacuously",
        graph.len()
    );
    for legacy in ["zuno-plugin", "zuno-plugin-sdk"] {
        assert!(
            !contains_package(&graph, legacy),
            "the shipped binary still depends on legacy crate `{legacy}`; the current \
             plugin host lives in `zuno-extension` and exposes no old Rust plugin ABI\n{}",
            inverted_path(legacy)
        );
    }
}

#[test]
fn the_shipped_binary_has_no_native_dynamic_plugin_loader() {
    let graph = default_graph(&["-p", "zuno-cli"]);
    let offenders = exact_matches(&graph, DYNAMIC_PLUGIN_LOADER_CRATES);
    assert!(
        offenders.is_empty(),
        "the shipped binary contains a native dynamic-library plugin loader: \
         {offenders:?}\nRuntime-loadable plugins must use WASI components or a \
         contained process.\nHow each one got in:\n{}",
        explain(&offenders)
    );
}

#[test]
fn the_default_workspace_graph_contains_the_wasi_component_host() {
    let graph = default_graph(&["--workspace"]);
    assert!(
        graph.len() >= MINIMUM_GRAPH_PACKAGES,
        "the workspace graph has only {} packages; this assertion would pass vacuously",
        graph.len()
    );
    for required in ["wasmtime", "wasmtime-wasi"] {
        assert!(
            contains_package(&graph, required),
            "the workspace graph has no `{required}`; the release graph is not \
             exercising the plugin runtime"
        );
    }
}

// ─── The unsafe gate ────────────────────────────────────────────────────────

/// Removes a trailing line comment so that prose *about* `unsafe` does not read
/// as a use of it. The `"` guard keeps a string literal containing `//` from
/// truncating the line; its only failure mode is a missed detection on a line that
/// opens a string and then writes `unsafe`, which is not valid Rust.
///
/// Same helper shape as `zuno-error/tests/no_anyhow_in_libraries.rs`, on purpose.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(at) if !line[..at].contains('"') => &line[..at],
        _ => line,
    }
}

/// True when a line uses `unsafe` as a Rust keyword rather than mentioning it.
///
/// Keyword positions only: `unsafe {`, `unsafe fn`, `unsafe impl`, `unsafe trait`,
/// `unsafe extern`, and any attempt to switch the lint off. The workspace sets
/// `unsafe_code = "forbid"`, so `#[allow(unsafe_code)]` cannot actually work —
/// but a crate that has not opted into the workspace lints would accept it, and
/// that combination is exactly what this pair of tests is for.
fn uses_unsafe_keyword(line: &str) -> bool {
    let code = strip_line_comment(line);
    let trimmed = code.trim_start();
    const KEYWORD_FORMS: &[&str] = &[
        "unsafe {",
        "unsafe fn",
        "unsafe impl",
        "unsafe trait",
        "unsafe extern",
        "unsafe_code",
    ];
    KEYWORD_FORMS.iter().any(|form| {
        // Leading position, or preceded by something that cannot make it an
        // identifier: `pub unsafe fn`, `= unsafe {`, `(unsafe {`.
        trimmed.starts_with(form)
            || code
                .match_indices(form)
                .any(|(at, _)| at == 0 || !is_identifier_char(code.as_bytes()[at - 1]))
    })
}

fn is_identifier_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Yields `crates/<name>/src/**/*.rs`. Tests are out of scope: this is about what
/// reaches the shipped artifact.
fn library_sources(root: &Path) -> Vec<(String, PathBuf)> {
    let crates_dir = root.join("crates");
    let mut found = Vec::new();
    let entries = std::fs::read_dir(&crates_dir).expect("crates/ is readable");
    for entry in entries.flatten() {
        let crate_dir = entry.path();
        let src = crate_dir.join("src");
        if !src.is_dir() {
            continue;
        }
        let name = crate_dir
            .file_name()
            .expect("crate directory has a name")
            .to_string_lossy()
            .into_owned();
        collect_rust_files(&src, &name, &mut found);
    }
    found.sort();
    found
}

fn first_party_rust_sources(root: &Path) -> Vec<(String, PathBuf)> {
    let crates_dir = root.join("crates");
    let mut found = Vec::new();
    let entries = std::fs::read_dir(&crates_dir).expect("crates/ is readable");
    for entry in entries.flatten() {
        let crate_dir = entry.path();
        if !crate_dir.is_dir() {
            continue;
        }
        let name = crate_dir
            .file_name()
            .expect("crate directory has a name")
            .to_string_lossy()
            .into_owned();
        collect_rust_files(&crate_dir, &name, &mut found);
    }
    found.sort();
    found
}

fn allow_attributes(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<_> = text.lines().collect();
    let mut attributes = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim_start();
        if !trimmed.starts_with("#[allow(") && !trimmed.starts_with("#![allow(") {
            index += 1;
            continue;
        }

        let start = index;
        let mut attribute = trimmed.to_owned();
        while !attribute.trim_end().ends_with(")]") {
            index += 1;
            assert!(
                index < lines.len(),
                "unterminated allow attribute beginning on line {}",
                start + 1
            );
            attribute.push('\n');
            attribute.push_str(lines[index].trim());
        }
        attributes.push((start + 1, attribute));
        index += 1;
    }
    attributes
}

fn allow_has_reason(attribute: &str) -> bool {
    attribute
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .contains("reason=")
}

fn collect_rust_files(dir: &Path, crate_name: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, crate_name, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push((crate_name.to_owned(), path));
        }
    }
}

#[test]
fn no_first_party_source_file_uses_unsafe() {
    let root = workspace_root();
    let sources = library_sources(&root);
    assert!(
        sources.len() >= MINIMUM_SOURCE_FILES,
        "scanned only {} source files under {}; the scan is looking in the wrong \
         place and would pass vacuously",
        sources.len(),
        root.join("crates").display()
    );

    let mut offenders = Vec::new();
    for (crate_name, path) in &sources {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for (index, line) in text.lines().enumerate() {
            if uses_unsafe_keyword(line) {
                offenders.push(format!(
                    "  {}:{}  [crate {crate_name}]\n    {}",
                    path.display(),
                    index + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "{} source line(s) use `unsafe`. This workspace sets \
         `unsafe_code = \"forbid\"`, so the shipped artifact contains no \
         first-party unsafe code and this scan keeps that true even for a crate \
         that has not inherited the lint:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

#[test]
fn every_first_party_lint_suppression_has_a_reason() {
    let root = workspace_root();
    let sources = first_party_rust_sources(&root);
    assert!(
        sources.len() >= MINIMUM_SOURCE_FILES,
        "scanned only {} Rust files under {}; the scan is looking in the wrong place",
        sources.len(),
        root.join("crates").display()
    );

    let mut offenders = Vec::new();
    for (crate_name, path) in &sources {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let relative = path
            .strip_prefix(&root)
            .expect("first-party source is below workspace root")
            .to_string_lossy();
        for (line, attribute) in allow_attributes(&text) {
            if allow_has_reason(&attribute) {
                continue;
            }
            offenders.push(format!(
                "  {relative}:{line} [crate {crate_name}]\n    {}",
                attribute.replace('\n', " ")
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "{} first-party `allow` attribute(s) lack `reason = ...`; justify each suppression at \
         the attribute rather than silently expanding the exception list:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// The load-bearing half of the unsafe guarantee.
///
/// `unsafe_code = "forbid"` is a *workspace* lint, and a workspace lint applies
/// only to crates that write `[lints] workspace = true`. A crate that omits it
/// compiles unsafe code silently, which makes the omission a defect rather than a
/// style nit — and makes it invisible to any amount of source scanning of the
/// other crates.
#[test]
fn every_workspace_member_inherits_the_workspace_lints() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let mut checked = 0_usize;
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&crates_dir).expect("crates/ is readable") {
        let manifest = entry.expect("readable dir entry").path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        checked += 1;
        let text = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
        if !inherits_workspace_lints(&text) {
            offenders.push(manifest.display().to_string());
        }
    }
    assert!(
        checked >= MINIMUM_CRATES,
        "checked only {checked} manifests under {}; the scan is looking in the \
         wrong place",
        crates_dir.display()
    );
    assert!(
        offenders.is_empty(),
        "{} crate manifest(s) do not contain `[lints]` + `workspace = true`, so \
         `unsafe_code = \"forbid\"` does not apply to them:\n  {}\n\n\
         Owner policy: this workspace keeps first-party `unsafe` at zero. Downgrading \
         the lint for a crate needs the owner's approval AND a comment in that crate's \
         manifest stating why safe Rust cannot express the operation \u{2014} see the two \
         existing precedents, `crates/zuno-paths/src/lib.rs` (avoiding \
         `std::env::set_var`) and the `portable-pty` rationale in the root \
         `Cargo.toml`. Any `unsafe` block that does land must carry the same \
         justification at its site.",
        offenders.len(),
        offenders.join("\n  ")
    );
}

/// The workspace's actual members, as sorted package names.
///
/// `cargo metadata --no-deps` rather than a `crates/` directory listing: membership
/// is what `[workspace] members` resolves to, so a directory without a manifest, or
/// one named in `exclude`, is correctly absent, and a member added from outside
/// `crates/*` is correctly present.
fn workspace_member_names() -> Vec<String> {
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace_root())
        .env("CARGO_NET_OFFLINE", "true")
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .output()
        .expect("cargo metadata must be runnable");
    assert!(
        output.status.success(),
        "cargo metadata failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata emits JSON");
    let mut names: Vec<String> = document["packages"]
        .as_array()
        .expect("cargo metadata --no-deps lists packages")
        .iter()
        .map(|package| {
            package["name"]
                .as_str()
                .expect("every package has a name")
                .to_owned()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The roster is a closed list, and `members = ["crates/*"]` cannot keep one.
///
/// The plan freezes an enumerated roster and requires any addition to be a
/// deliberate change with a matching count and fixture update. The glob makes an
/// addition invisible: `zuno-process` and `zuno-reaping-fixture` joined the workspace
/// with `crates.expected` untouched and every gate still green, because the only
/// crate-count assertion in this file was a *floor*. This compares the two sets
/// exactly, in both directions, so the next addition fails here until the roster is
/// amended on purpose.
#[test]
fn the_workspace_roster_matches_the_declared_crate_list() {
    let root = workspace_root();
    let fixture = root.join(ROSTER_FIXTURE);
    let declared_text = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|e| panic!("read {}: {e}", fixture.display()));
    let declared: BTreeSet<&str> = declared_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let actual_names = workspace_member_names();
    let actual: BTreeSet<&str> = actual_names.iter().map(String::as_str).collect();

    assert!(
        actual.len() >= MINIMUM_CRATES,
        "cargo metadata reported only {} workspace members; the query is pointed at \
         the wrong workspace and this assertion would pass vacuously",
        actual.len()
    );

    let undeclared: Vec<&&str> = actual.difference(&declared).collect();
    let missing: Vec<&&str> = declared.difference(&actual).collect();
    assert!(
        undeclared.is_empty() && missing.is_empty(),
        "the workspace roster and {} disagree. The roster is a closed list: a new \
         crate is a deliberate amendment to {}, to the enumeration in \
         the project plan and to its stated count, in the same commit \
         — never a silent consequence of `members = [\"crates/*\"]`.\n  \
         in the workspace but not declared: {undeclared:?}\n  \
         declared but not in the workspace: {missing:?}\n  \
         declared {} / actual {}",
        ROSTER_FIXTURE,
        ROSTER_FIXTURE,
        declared.len(),
        actual.len()
    );
}

/// Whether a manifest opts into the workspace lint table.
///
/// Textual because it must report on a crate that does not currently compile, and
/// because a full TOML parse is not needed to answer a yes/no question about two
/// adjacent lines. Both spellings cargo accepts are matched.
fn inherits_workspace_lints(manifest: &str) -> bool {
    if manifest
        .lines()
        .any(|line| line.trim() == "lints.workspace = true")
    {
        return true;
    }
    let mut in_lints = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lints = trimmed == "[lints]";
            continue;
        }
        if in_lints && trimmed.replace(' ', "") == "workspace=true" {
            return true;
        }
    }
    false
}

// ─── The pipeline's own invariants ──────────────────────────────────────────

fn workflow(name: &str) -> String {
    let path = workspace_root().join(".github/workflows").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The lines of a workflow file that are inside the top-level `jobs:` mapping.
///
/// A textual scan rather than a YAML parse: it keeps `make ci` free of a Python or
/// extra-crate dependency, and its failure mode is loud — a reformatted workflow
/// makes an assertion fail and someone fixes it, which is the right way round for
/// a gate.
///
/// Starting at `jobs:` is not cosmetic. `on:` also contains two-space keys
/// (`push:`, `pull_request:`) and `workflow_dispatch.inputs` can contain a key
/// with the same name as a job — this file's `publish` input and `publish` job
/// collide exactly that way. Scanning the whole file mistakes both for jobs.
fn job_region(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .skip_while(|line| line.trim_end() != "jobs:")
        .skip(1)
}

/// Whether `line` is a job header, i.e. a key at exactly two-space indentation.
fn job_header(line: &str) -> Option<&str> {
    if !line.starts_with("  ") || line.starts_with("   ") || line.trim_start().starts_with('#') {
        return None;
    }
    line.trim().strip_suffix(':')
}

/// Every job name declared in a workflow, in file order.
fn job_names(text: &str) -> BTreeSet<String> {
    job_region(text)
        .filter_map(job_header)
        .map(str::to_owned)
        .collect()
}

/// The lines belonging to one job, header excluded.
fn job_body<'a>(text: &'a str, job: &str) -> Vec<&'a str> {
    let mut body = Vec::new();
    let mut inside = false;
    for line in job_region(text) {
        match job_header(line) {
            Some(name) => inside = name == job,
            None if inside => body.push(line),
            None => {}
        }
    }
    body
}

/// The `needs:` list of one job, as a set of job names.
///
/// Handles the flow spelling (`needs: [a, b]`) and the single-value spelling
/// (`needs: a`), which are the two this repository uses.
fn job_needs(text: &str, job: &str) -> BTreeSet<String> {
    job_body(text, job)
        .iter()
        .find_map(|line| line.trim().strip_prefix("needs:"))
        .map(|list| {
            list.trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The `- target: <triple>` values inside one job's matrix.
fn matrix_targets(text: &str, job: &str) -> BTreeSet<String> {
    job_body(text, job)
        .iter()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .map(|value| value.trim().to_owned())
        .collect()
}

/// The label a job uses to route itself to `project`'s CodeBuild runner.
fn project_label(project: &str) -> String {
    format!("codebuild-{project}-${{{{ github.run_id }}}}-${{{{ github.run_attempt }}}}")
}

fn codebuild_label_sets(text: &str) -> Vec<Vec<String>> {
    let project_labels: Vec<String> = RUNNER_PROJECTS.iter().map(|p| project_label(p)).collect();

    let lines: Vec<&str> = text.lines().collect();
    let mut sets = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let Some(label) = trimmed.strip_prefix("- ") else {
            continue;
        };
        if !project_labels.iter().any(|known| known == label) {
            continue;
        }

        let indentation = line.len() - trimmed.len();
        let labels = lines[index..]
            .iter()
            .take_while(|candidate| {
                candidate.len() - candidate.trim_start().len() == indentation
                    && candidate.trim_start().starts_with("- ")
            })
            .map(|candidate| candidate.trim_start()[2..].to_owned())
            .collect();
        sets.push(labels);
    }
    sets
}

fn matrix_entry<'a>(text: &'a str, job: &str, target: &str) -> Vec<&'a str> {
    let body = job_body(text, job);
    let target_header = format!("- target: {target}");
    let mut entry = Vec::new();
    let mut inside = false;
    for line in body {
        let trimmed = line.trim();
        if trimmed.starts_with("- target: ") {
            inside = trimmed == target_header;
        }
        if inside {
            entry.push(line);
        }
    }
    entry
}

#[test]
fn the_release_matrix_builds_every_target_the_project_ships() {
    let text = workflow("release.yml");
    let built = matrix_targets(&text, "build");
    let expected: BTreeSet<String> = RELEASE_TARGETS.iter().map(|t| (*t).to_owned()).collect();
    assert_eq!(
        built, expected,
        "release.yml's `build` matrix does not name exactly the five shipped targets"
    );
}

/// The assertion behind "must not ship an artifact that was never executed".
///
/// The two matrices are separate because the `aarch64-unknown-linux-musl` archive
/// is cross-linked on x86_64 and can only be *run* on an arm64 runner, so the
/// smoke leg has to be a different job on a different machine. That separation is
/// also the failure mode this test exists for: a target added to `build` and
/// forgotten in `smoke` would ship unexecuted with CI still green.
#[test]
fn every_built_target_is_also_smoke_tested() {
    let text = workflow("release.yml");
    let built = matrix_targets(&text, "build");
    let smoked = matrix_targets(&text, "smoke");
    assert_eq!(
        built.len(),
        RELEASE_TARGETS.len(),
        "expected {} build targets, parsed {built:?}",
        RELEASE_TARGETS.len()
    );
    let unexecuted: Vec<&String> = built.difference(&smoked).collect();
    assert!(
        unexecuted.is_empty(),
        "release.yml builds {unexecuted:?} but never runs the resulting binary. \
         An artifact that was never executed must not ship: add it to the `smoke` \
         matrix with a runner of its own architecture."
    );
    let unbuilt: Vec<&String> = smoked.difference(&built).collect();
    assert!(
        unbuilt.is_empty(),
        "release.yml smoke-tests {unbuilt:?}, which nothing builds; that job would \
         fail on a missing artifact"
    );
}

#[test]
fn release_toolchains_are_installed_once_and_reused_by_smoke_drivers() {
    let text = workflow("release.yml");
    let version = job_body(&text, "version").join("\n");
    assert!(
        !version.contains("dtolnay/rust-toolchain") && !version.contains("cargo metadata"),
        "the version resolver must read Cargo.toml without installing Rust:\n{version}"
    );
    for required in ["awk '", "workspace\\.package", "Cargo.toml"] {
        assert!(
            version.contains(required),
            "the toolchain-free version resolver is missing {required:?}:\n{version}"
        );
    }

    let build = job_body(&text, "build").join("\n");
    assert_eq!(
        build.matches("dtolnay/rust-toolchain@").count(),
        1,
        "each build matrix leg must install Rust exactly once"
    );
    assert_eq!(
        build
            .matches("name: Install MSVC build tools (Windows container)")
            .count(),
        1,
        "the Windows build leg must install MSVC exactly once"
    );
    for required in [
        "Build the smoke driver (zigbuild)",
        "Build the smoke driver (native)",
        "name: smoke-${{ matrix.target }}",
        "path: smoke/*",
    ] {
        assert!(
            build.contains(required),
            "the build job does not prepare the reusable smoke driver field {required:?}"
        );
    }

    let smoke = job_body(&text, "smoke").join("\n");
    for forbidden in [
        "dtolnay/rust-toolchain@",
        "Install MSVC build tools",
        "Swatinem/rust-cache@",
        "cargo build --locked",
        "cargo zigbuild --locked",
    ] {
        assert!(
            !smoke.contains(forbidden),
            "the smoke job repeats build toolchain work {forbidden:?}:\n{smoke}"
        );
    }
    for required in [
        "name: smoke-${{ matrix.target }}",
        "path: smoke",
        "./smoke/zuno-smoke",
        ".\\smoke\\zuno-smoke.exe",
        "--cassette-root \"$GITHUB_WORKSPACE/packaging/smoke/cassettes\"",
        "--models \"$GITHUB_WORKSPACE/crates/zuno-llm/tests/fixtures/models-dev-pinned.json\"",
        "--cassette-root \"$env:GITHUB_WORKSPACE\\packaging\\smoke\\cassettes\"",
        "--models \"$env:GITHUB_WORKSPACE\\crates\\zuno-llm\\tests\\fixtures\\models-dev-pinned.json\"",
    ] {
        assert!(
            smoke.contains(required),
            "the smoke job does not consume the reusable driver correctly; missing \
             {required:?}"
        );
    }
}

#[test]
fn every_codebuild_job_has_a_unique_label_set() {
    // Was 11 when only Linux jobs ran on CodeBuild. The four non-Linux release
    // legs that used to sit on GitHub-hosted runners are now two macOS build legs,
    // two macOS smoke legs, one Windows build leg and one Windows smoke leg — six
    // more routed jobs, less the two `aarch64-pc-windows-msvc` legs that were
    // dropped because no CodeBuild machine can execute that artifact.
    //
    // 17 after the seven-second supply-chain check moved into the existing `test`
    // job. Release still has separate build and execution legs because the
    // aarch64 Linux artifact crosses from the x86_64 build machine to a native ARM
    // runner, but the smoke legs consume precompiled drivers and install no Rust
    // or MSVC toolchain.
    const EXPECTED_MIGRATED_JOBS: usize = 17;

    let workflows = [
        ("ci.yml", workflow("ci.yml")),
        ("release.yml", workflow("release.yml")),
    ];
    let mut named_sets = Vec::new();
    for (name, text) in &workflows {
        for labels in codebuild_label_sets(text) {
            println!("{name}: {}", labels.join(" + "));
            named_sets.push((name, labels));
        }
    }

    assert_eq!(
        named_sets.len(),
        EXPECTED_MIGRATED_JOBS,
        "expected one CodeBuild label set for each job or matrix leg"
    );

    let known_project_labels: Vec<String> =
        RUNNER_PROJECTS.iter().map(|p| project_label(p)).collect();
    let mut routing_labels = BTreeSet::new();
    let mut complete_sets = BTreeSet::new();
    for (workflow_name, labels) in &named_sets {
        let first = labels.first().map(String::as_str).unwrap_or_default();
        assert!(
            known_project_labels.iter().any(|known| known == first),
            "{workflow_name} routes a job to {first:?}, which is not one of the \
             CodeBuild runner projects {RUNNER_PROJECTS:?}"
        );
        let routing_label = labels
            .get(1)
            .unwrap_or_else(|| panic!("{workflow_name} has no unique second label in {labels:?}"));
        assert!(
            routing_label.starts_with("zuno-ci-") || routing_label.starts_with("zuno-release-"),
            "{workflow_name}'s second label is not a Zuno job identity: {labels:?}"
        );
        assert!(
            routing_labels.insert(routing_label.clone()),
            "duplicate CodeBuild routing label {routing_label:?}"
        );
        assert!(
            complete_sets.insert(labels.clone()),
            "duplicate CodeBuild label set {labels:?}"
        );
    }

    assert_eq!(
        workflows[1]
            .1
            .matches("runs-on: ${{ matrix.runs_on }}")
            .count(),
        2,
        "both release matrices must select their per-entry label arrays"
    );
}

#[test]
fn ci_runs_before_the_protected_merge_without_a_duplicate_push_run() {
    let text = workflow("ci.yml");
    let trigger_region = text
        .split_once("\njobs:")
        .map(|(before_jobs, _)| before_jobs)
        .expect("ci.yml declares jobs");
    assert!(
        trigger_region
            .lines()
            .all(|line| line.trim_end() != "push:"),
        "ci.yml still runs on the protected main merge commit and duplicates the \
         pull-request result"
    );
    for required in ["pull_request:", "workflow_dispatch:"] {
        assert!(
            trigger_region.lines().any(|line| line.trim() == required),
            "ci.yml is missing the {required} trigger"
        );
    }

    let jobs = job_names(&text);
    assert!(
        !jobs.contains("supply-chain"),
        "the seven-second supply-chain check still starts a separate Rust runner"
    );
    let test = job_body(&text, "test").join("\n");
    for required in ["tool: cargo-deny", "cargo deny --all-features check"] {
        assert!(
            test.contains(required),
            "the consolidated test job lost the supply-chain gate {required:?}"
        );
    }
}

/// The release path must not depend on a GitHub-hosted runner.
///
/// This is the assertion that would have caught the state this file was written
/// out of: `release.yml` named `macos-15-intel`, `macos-latest`, `windows-latest`
/// and `windows-11-arm`, this account has no Actions minutes, and `publish` needs
/// every build and smoke leg — so the pipeline could never once reach publication,
/// and nothing failed loudly enough to say so. A hosted label is easy to
/// reintroduce while debugging a runner problem, which is exactly when it must be
/// refused.
///
/// Scoped to `release.yml` on purpose. `ci.yml` keeps one hosted `windows` job
/// behind `if: vars.HOSTED_RUNNERS == 'true'`, off by default, and that job
/// gates nothing that ships.
#[test]
fn the_release_path_uses_no_github_hosted_runner() {
    let text = workflow("release.yml");
    const HOSTED_LABELS: &[&str] = &[
        "macos-latest",
        "macos-15",
        "macos-14",
        "macos-13",
        "windows-latest",
        "windows-2022",
        "windows-2019",
        "windows-11-arm",
        "ubuntu-latest",
        "ubuntu-24.04",
        "ubuntu-22.04",
    ];

    let mut offenders = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let code = line.split('#').next().unwrap_or_default();
        for hosted in HOSTED_LABELS {
            if code
                .split_whitespace()
                .any(|word| word.trim_start_matches("- ").trim_matches(['"', '\'']) == *hosted)
            {
                offenders.push(format!("  release.yml:{}: {}", index + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "release.yml routes a job to a GitHub-hosted runner:\n{}\n\
         This repository's Actions minutes are exhausted, so such a job never \
         starts, and `publish` needs every build and smoke leg — one hosted label \
         means no release can ever be published. Route it to one of \
         {RUNNER_PROJECTS:?} instead.",
        offenders.join("\n")
    );

    // The positive half: no hosted label proves nothing if the file stopped
    // selecting runners at all, or if a whole matrix leg vanished.
    for project in RUNNER_PROJECTS {
        let label = project_label(project);
        assert!(
            text.contains(&label),
            "release.yml never routes to {project}; with the hosted runners gone \
             there would be no machine for that platform's legs at all"
        );
    }
}

#[test]
fn release_matrices_use_the_effective_runner_fields() {
    let linux_project_label = project_label("zuno-runner");
    let project_label = linux_project_label.as_str();

    let text = workflow("release.yml");
    for job in ["build", "smoke"] {
        let legacy_fields: Vec<_> = job_body(&text, job)
            .into_iter()
            .filter(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("os:") || trimmed.starts_with("runner:")
            })
            .collect();
        assert!(
            legacy_fields.is_empty(),
            "release.yml's `{job}` matrix retains fields that do not select the runner: \
             {legacy_fields:?}; `runs_on` is the effective field"
        );
    }

    let x86_smoke = matrix_entry(&text, "smoke", "x86_64-unknown-linux-musl").join("\n");
    for required in [project_label, "zuno-release-smoke-x86_64-linux"] {
        assert!(
            x86_smoke.contains(required),
            "the x86_64 Linux smoke leg is missing CodeBuild label {required:?}"
        );
    }

    let arm_smoke = matrix_entry(&text, "smoke", "aarch64-unknown-linux-musl").join("\n");
    for required in [
        project_label,
        "zuno-release-smoke-aarch64-linux",
        "image:arm-3.0",
        "instance-size:large",
    ] {
        assert!(
            arm_smoke.contains(required),
            "the aarch64 Linux smoke leg is missing CodeBuild label {required:?}"
        );
    }
}

/// Also from "must not ship an artifact that was never executed": publication has
/// to depend on the smoke job, not merely coexist with it.
#[test]
fn publication_depends_on_the_smoke_job() {
    let text = workflow("release.yml");
    let publish_needs = job_needs(&text, "publish");
    assert!(
        !publish_needs.is_empty(),
        "release.yml's publish job declares no `needs:`, so nothing gates it"
    );
    for required in ["build", "smoke"] {
        assert!(
            publish_needs.contains(required),
            "release.yml's publish job does not need `{required}` (needs: \
             {publish_needs:?}); an unexecuted artifact could then be published"
        );
    }
}

#[test]
fn publication_includes_the_checksum_manifest_required_by_self_update() {
    let text = workflow("release.yml");
    let checksum_needs = job_needs(&text, "checksums");
    for required in ["build", "smoke"] {
        assert!(
            checksum_needs.contains(required),
            "the checksums job must depend on `{required}`: {checksum_needs:?}"
        );
    }

    let publish_needs = job_needs(&text, "publish");
    assert!(
        publish_needs.contains("checksums"),
        "publish can run without the checksum job: {publish_needs:?}"
    );
    let checksums = job_body(&text, "checksums").join("\n");
    for required in ["sha256sum \"$archive\"", "SHA256SUMS"] {
        assert!(
            checksums.contains(required),
            "the checksums job does not produce {required:?}"
        );
    }
    let publish = job_body(&text, "publish").join("\n");
    assert!(
        publish.contains("dist/SHA256SUMS"),
        "the GitHub Release does not attach SHA256SUMS"
    );
}

/// The constraint the corrected plan wording actually states: no *per-target* C
/// cross-toolchain. A C compiler for the host is required and expected — bundled
/// SQLite and `aws-lc-sys` both compile C — so this scans for the specific
/// mechanisms that were ruled out, not for compilation of C in general.
///
/// Scoped to the `build` job, because cross-compilation happens only there. The
/// `smoke` job installs a runtime dependency of the artifact under test — the
/// bubblewrap the OS sandbox backend requires — which is a package for the *host*
/// that runs the binary, not a toolchain for a *target* it is built for. Scanning
/// the whole file conflated the two and made a legitimate runtime install
/// indistinguishable from the cross mechanism this rules out.
#[test]
fn the_musl_legs_use_zig_and_no_cross_toolchain() {
    let text = workflow("release.yml");
    let build = job_body(&text, "build").join("\n");
    let mut offenders = Vec::new();
    for (index, line) in build.lines().enumerate() {
        let code = line.split('#').next().unwrap_or_default();
        let lowered = code.to_ascii_lowercase();
        let banned = [
            ("apt-get", "a per-target apt package"),
            ("apt install", "a per-target apt package"),
            ("docker ", "a docker image"),
            ("cross build", "the `cross` docker wrapper"),
            ("gcc-aarch64", "a system cross-gcc"),
            ("gcc-x86-64", "a system cross-gcc"),
            ("musl-tools", "a system musl toolchain"),
            ("mingw", "a system mingw toolchain"),
        ];
        for (needle, what) in banned {
            if lowered.contains(needle) {
                offenders.push(format!(
                    "  release.yml build job, line {}: {what} (matched {needle:?})\n    {}",
                    index + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "release.yml's build job reaches for a per-target C cross-toolchain. Zig plus \
         cargo-zigbuild is a hermetic C cross-compiler in one download and is the \
         only cross mechanism this pipeline may use:\n{}",
        offenders.join("\n")
    );

    // The positive half: the ruled-out mechanisms being absent proves nothing if
    // the sanctioned one is absent too.
    for required in ["mlugg/setup-zig", "cargo-zigbuild", "cargo zigbuild"] {
        assert!(
            text.contains(required),
            "release.yml does not mention `{required}`; the two musl legs cannot \
             cross-compile this workspace's C dependencies without Zig"
        );
    }
    for (musl, routing_label) in [
        (
            "x86_64-unknown-linux-musl",
            "zuno-release-build-x86_64-linux",
        ),
        (
            "aarch64-unknown-linux-musl",
            "zuno-release-build-aarch64-linux",
        ),
    ] {
        let entry = matrix_entry(&text, "build", musl).join("\n");
        for required in ["use_zigbuild: true", routing_label] {
            assert!(
                entry.contains(required),
                "release.yml's `{musl}` build entry is missing {required:?}; it \
                 would not use Zig on its dedicated CodeBuild route"
            );
        }
    }

    // Narrowing the scan above to `build` is only safe while the one job still
    // allowed to install packages uses that only for the artifact's own runtime
    // dependency. A toolchain package appearing here would be the same mistake
    // wearing a different job name.
    //
    // Every package manager the workflow can reach is checked, not just apt. The
    // two Linux legs run different distributions — Ubuntu for x86_64, Amazon Linux
    // for the `image:arm-3.0` aarch64 route — so the install step branches, and a
    // guard that only understood apt would have silently stopped covering half the
    // matrix the moment that branch was added.
    const SANDBOX_PACKAGE: &str = "bubblewrap";
    const INSTALL_VERBS: &[&str] = &[
        "apt-get install",
        "apt install",
        "dnf install",
        "yum install",
    ];
    for line in job_body(&text, "smoke") {
        let code = line
            .split('#')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        // Only the install verb matters. `apt-get update` names no package, and
        // accepting its presence as justification is what made an earlier version of
        // this assertion vacuous: the injected `apt-get update && apt-get install
        // musl-tools` satisfied it on the update half alone.
        let Some(installed) = INSTALL_VERBS
            .iter()
            .find_map(|verb| code.split_once(verb).map(|(_, rest)| rest))
        else {
            continue;
        };
        let packages: Vec<&str> = installed
            .split_whitespace()
            .filter(|word| !word.starts_with('-') && *word != "&&")
            .collect();
        assert!(
            !packages.is_empty() && packages.iter().all(|pkg| *pkg == SANDBOX_PACKAGE),
            "release.yml's smoke job installs package(s) {packages:?}; only \
             {SANDBOX_PACKAGE:?} is justified here (the OS sandbox backend the smoked \
             binary requires). A toolchain package in this job is the same per-target \
             cross mechanism the build job forbids, wearing a different job name:\n    {}",
            line.trim()
        );
    }
}

/// The gap GitHub Actions cannot close from inside a workflow: the `needs:` list
/// of the required status check is where a new job silently fails to be required.
#[test]
fn the_ci_gate_requires_every_job_in_the_workflow() {
    let text = workflow("ci.yml");
    let jobs = job_names(&text);
    assert!(
        jobs.len() >= 4,
        "parsed only {jobs:?} as ci.yml's jobs; the parse is wrong and this \
         assertion would pass vacuously"
    );
    assert!(
        jobs.contains("ci-success"),
        "ci.yml has no `ci-success` job; there is then no single required status \
         check to protect the branch with"
    );
    assert!(
        !jobs.contains("push") && !jobs.contains("pull_request"),
        "the job parse leaked `on:` triggers into {jobs:?}; it is reading the wrong \
         region of the file"
    );

    let required = job_needs(&text, "ci-success");
    let mut expected = jobs.clone();
    expected.remove("ci-success");
    let missing: Vec<&String> = expected.difference(&required).collect();
    assert!(
        missing.is_empty(),
        "ci.yml defines job(s) {missing:?} that `ci-success` does not require, so \
         they could fail without turning the required check red. Add them to \
         `needs:`, or state here why one is informational."
    );
}

/// The five targets the plan names, plus the ones CI invokes by name. A workflow
/// step calling a make target that does not exist fails only when that workflow
/// runs, which for the release path could be months later.
#[test]
fn the_makefile_exposes_every_target_the_plan_and_ci_require() {
    let path = workspace_root().join("Makefile");
    let text = std::fs::read_to_string(&path).expect("the workspace has a Makefile");
    let declared: BTreeSet<&str> = text
        .lines()
        .filter(|line| {
            !line.starts_with('\t') && line.contains(':') && !line.trim_start().starts_with('#')
        })
        .filter_map(|line| line.split(':').next())
        .flat_map(str::split_whitespace)
        .collect();

    for required in [
        // The plan's five.
        "fmt",
        "lint",
        "test",
        "ci",
        "release",
        // Invoked by name from .github/workflows/ci.yml.
        "fmt-check",
        "deny",
        "smoke-artifact",
    ] {
        assert!(
            declared.contains(required),
            "the Makefile has no `{required}` target; declared: {declared:?}"
        );
    }

    // `ci` must actually run the gates rather than being an empty alias.
    let ci_line = text
        .lines()
        .find(|line| line.starts_with("ci:"))
        .expect("the Makefile declares a `ci` target");
    for prerequisite in ["fmt-check", "lint", "test", "deny"] {
        assert!(
            ci_line.contains(prerequisite),
            "`make ci` does not run `{prerequisite}` ({ci_line}); the local gate \
             and the CI gate would then check different things"
        );
    }
}

/// `make build` is the local handoff path: after Cargo succeeds, one stable
/// executable must be available without knowing Cargo's target layout.
#[test]
fn make_build_stages_a_directly_runnable_binary_in_dist() {
    let output = Command::new("make")
        .current_dir(workspace_root())
        .env_remove("CARGO_TARGET_DIR")
        .args(["-n", "build"])
        .output()
        .expect("make must be runnable because CI uses the same Makefile");
    assert!(
        output.status.success(),
        "make -n build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let commands = String::from_utf8(output.stdout).expect("make output is UTF-8");
    let executable = if cfg!(windows) { "zuno.exe" } else { "zuno" };
    for required in [
        format!("target/debug/{executable}"),
        format!("dist/{executable}.tmp"),
        format!("dist/{executable}"),
    ] {
        assert!(
            commands.contains(&required),
            "`make build` does not stage `{required}`:\n{commands}"
        );
    }
    assert!(
        commands.contains("mv -f"),
        "`make build` must publish the completed copy atomically:\n{commands}"
    );
}

// ─── The committed cassette ─────────────────────────────────────────────────

/// The smoke test replays a cassette committed under `packaging/smoke/cassettes/`
/// because a CI runner has this repository and nothing else — it cannot reach the
/// oracle checkout that `zuno_testkit::cassette::recordings_root` looks for.
///
/// A copy can drift from its source, so on any machine that has both, this asserts
/// they are byte-identical. When no oracle tree is reachable it prints a named
/// skip: a silent pass here would let the copy rot unnoticed, and a hard failure
/// would break the suite on every CI runner.
#[test]
fn committed_smoke_cassette_matches_the_oracle_recording() {
    let committed = workspace_root()
        .join("packaging/smoke/cassettes")
        .join(format!("{SMOKE_CASSETTE}.json"));
    let ours =
        std::fs::read(&committed).unwrap_or_else(|e| panic!("read {}: {e}", committed.display()));
    assert!(
        ours.len() > 1_000,
        "{} is {} bytes; that is not a recorded tool loop",
        committed.display(),
        ours.len()
    );

    let Some(root) = zuno_testkit::recordings_root_or_skip(
        "committed_smoke_cassette_matches_the_oracle_recording",
        "the committed copy was checked for shape only, not byte parity",
    ) else {
        return;
    };
    let theirs_path = root.join(format!("{SMOKE_CASSETTE}.json"));
    let theirs = std::fs::read(&theirs_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", theirs_path.display()));
    assert_eq!(
        ours,
        theirs,
        "the committed smoke cassette has drifted from the oracle recording.\n\
         committed: {} ({} bytes)\n\
         oracle:    {} ({} bytes)\n\
         Re-copy it verbatim and update the SHA-256 in \
         packaging/smoke/cassettes/PROVENANCE.md.",
        committed.display(),
        ours.len(),
        theirs_path.display(),
        theirs.len()
    );
}

/// The committed cassette must be a real recording end to end: version 1, two HTTP
/// interactions, and no authored bytes. If it were hand-written the smoke test
/// would prove the binary can talk to a fixture we invented rather than to bytes a
/// real provider sent.
#[test]
fn the_committed_smoke_cassette_is_a_two_turn_recording() {
    let root = workspace_root().join("packaging/smoke/cassettes");
    let player = zuno_testkit::CassettePlayer::load(&root, SMOKE_CASSETTE)
        .expect("the committed smoke cassette loads");
    let interactions: Vec<_> = player.cassette().http_interactions().collect();
    assert_eq!(
        interactions.len(),
        2,
        "the smoke cassette has {} HTTP interaction(s); the tool loop needs the \
         tool-call turn and the continuation",
        interactions.len()
    );
    assert_eq!(player.cassette().version, 1);
    assert_eq!(player.cassette().recorded_name(), Some(SMOKE_CASSETTE));

    let scenario = zuno_testkit::Scenario::new("provenance-check")
        .from_cassette(SMOKE_CASSETTE, player.cassette())
        .expect("the committed cassette builds a scenario");
    assert_eq!(scenario.len(), 2);
}

// ─── Self-tests for the scanners ────────────────────────────────────────────

#[test]
fn the_unsafe_scanner_detects_every_keyword_position() {
    for case in [
        "unsafe { *ptr }",
        "    let value = unsafe { *ptr };",
        "pub unsafe fn danger() {}",
        "unsafe impl Send for Handle {}",
        "unsafe trait Contract {}",
        "unsafe extern \"C\" fn callback() {}",
        "#[allow(unsafe_code)]",
        "#![allow(unsafe_code)]",
    ] {
        assert!(uses_unsafe_keyword(case), "scanner missed {case:?}");
    }
}

#[test]
fn the_unsafe_scanner_ignores_prose_and_identifiers() {
    for case in [
        "//! `std::env::set_var` is `unsafe` and this workspace forbids it",
        "    /// forbids `unsafe_code`.",
        "// unsafe { *ptr }",
        "let intent = \"unsafe\";",
        "fn not_unsafe_fn() {}",
        "let is_unsafe_impl = false;",
        "",
    ] {
        assert!(
            !uses_unsafe_keyword(case),
            "scanner falsely accused {case:?}"
        );
    }
}

#[test]
fn a_string_literal_containing_a_comment_marker_does_not_hide_an_unsafe_block() {
    assert!(
        uses_unsafe_keyword("let doc = \"https://example.com\"; unsafe { ptr.read() }"),
        "a string literal containing // must not truncate the scanned line"
    );
}

#[test]
fn the_lint_inheritance_scanner_accepts_both_spellings_and_rejects_absence() {
    assert!(inherits_workspace_lints("[lints]\nworkspace = true\n"));
    assert!(inherits_workspace_lints("[lints]\nworkspace=true\n"));
    assert!(inherits_workspace_lints("lints.workspace = true\n"));
    assert!(!inherits_workspace_lints(
        "[dependencies]\nworkspace = true\n"
    ));
    assert!(!inherits_workspace_lints(
        "[lints.rust]\nunsafe_code = \"allow\"\n"
    ));
    assert!(!inherits_workspace_lints("[package]\nname = \"x\"\n"));
}

#[test]
fn the_matrix_parser_reads_only_the_named_job() {
    let text = "\
jobs:
  build:
    strategy:
      matrix:
        include:
          - target: one
          - target: two
  smoke:
    strategy:
      matrix:
        include:
          - target: two
";
    assert_eq!(
        matrix_targets(text, "build"),
        ["one", "two"].iter().map(|s| (*s).to_owned()).collect()
    );
    assert_eq!(
        matrix_targets(text, "smoke"),
        ["two"].iter().map(|s| (*s).to_owned()).collect()
    );
    assert_eq!(matrix_targets(text, "absent"), BTreeSet::new());
}

#[test]
fn the_package_matcher_does_not_confuse_a_prefix_for_a_package() {
    let graph: Vec<String> = ["openssl-probe v0.2.1", "rustls v0.23.43", "zuno-cli v0.1.0"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert!(!contains_package(&graph, "openssl"));
    assert!(contains_package(&graph, "openssl-probe"));
    assert!(exact_matches(&graph, OPENSSL_CRATES).is_empty());
    assert!(contains_package(&graph, "rustls"));
    assert!(!contains_package(&graph, "rust"));
}

/// Guards the guard: a real OpenSSL line must be caught. Without this the
/// no-OpenSSL test could be passing because the matcher never matches anything.
#[test]
fn the_package_matcher_catches_a_real_openssl_entry() {
    let mut cases: BTreeMap<&str, &str> = BTreeMap::new();
    cases.insert("openssl v0.10.68", "openssl");
    cases.insert("openssl-sys v0.9.104", "openssl-sys");
    cases.insert("native-tls v0.2.12", "native-tls");
    cases.insert("openssl-src v300.4.0", "openssl-src");
    for (line, expected) in cases {
        let graph = vec![line.to_owned(), "rustls v0.23.43".to_owned()];
        let hits = exact_matches(&graph, OPENSSL_CRATES);
        assert_eq!(
            hits.len(),
            1,
            "the matcher did not flag {line:?} (expected {expected})"
        );
    }
    for line in ["wasmtime v47.0.3", "cranelift-codegen v0.120.0"] {
        let graph = vec![line.to_owned()];
        assert_eq!(
            family_matches(&graph, WASM_RUNTIME_FAMILIES).len(),
            1,
            "the matcher did not flag {line:?}"
        );
    }
}

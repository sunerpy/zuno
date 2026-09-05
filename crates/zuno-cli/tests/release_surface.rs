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

#[cfg(unix)]
use sha2::{Digest, Sha256};

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
const RELEASE_TARGETS: [&str; 6] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

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
    let graph = default_graph(&["-p", "zuno"]);
    assert!(
        graph.len() >= MINIMUM_GRAPH_PACKAGES,
        "the graph for zuno has only {} packages; cargo tree returned something \
         unexpected and this assertion would pass vacuously",
        graph.len()
    );
    assert!(
        contains_package(&graph, "zuno"),
        "cargo tree did not report zuno itself; the graph is not the one intended"
    );

    let offenders = exact_matches(&graph, OPENSSL_CRATES);
    assert!(
        offenders.is_empty(),
        "the default zuno graph contains OpenSSL: {offenders:?}\n\
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
            "the default zuno graph has no `{required}`; the no-OpenSSL result \
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
    let graph = default_graph(&["-p", "zuno"]);
    assert!(
        graph.len() >= MINIMUM_GRAPH_PACKAGES,
        "the graph for zuno has only {} packages; this assertion would pass vacuously",
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
    let graph = default_graph(&["-p", "zuno"]);
    assert!(
        graph.len() >= MINIMUM_GRAPH_PACKAGES,
        "the graph for zuno has only {} packages; this assertion would pass vacuously",
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
    let graph = default_graph(&["-p", "zuno"]);
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

fn workspace_metadata() -> serde_json::Value {
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
    serde_json::from_slice(&output.stdout).expect("cargo metadata emits JSON")
}

#[test]
fn workspace_crates_are_private_and_release_is_binary_only() {
    let metadata = workspace_metadata();
    let packages = metadata["packages"]
        .as_array()
        .expect("cargo metadata lists packages");
    assert!(
        packages
            .iter()
            .all(|package| package["publish"] == serde_json::json!([])),
        "every workspace package must remain private; Zuno is distributed as prebuilt \
         GitHub Release archives"
    );

    let zuno = packages
        .iter()
        .find(|package| package["name"] == "zuno")
        .expect("the release binary package is named zuno");
    assert!(
        packages.iter().all(|package| package["name"] != "zuno-cli"),
        "the old package name zuno-cli is still present in cargo metadata"
    );
    assert!(
        zuno["targets"].as_array().is_some_and(|targets| {
            targets.iter().any(|target| {
                target["name"] == "zuno"
                    && target["kind"]
                        .as_array()
                        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
            })
        }),
        "the release package zuno does not expose the zuno binary"
    );

    let manifest =
        std::fs::read_to_string(workspace_root().join("Cargo.toml")).expect("root manifest");
    assert!(
        manifest.contains("publish = false"),
        "the workspace manifest must fail closed against cargo publish"
    );

    let publish_workflow = workspace_root().join(".github/workflows/publish-crates.yml");
    let publish_script = workspace_root().join(".github/scripts/publish-crates.py");
    assert!(
        !publish_workflow.exists() && !publish_script.exists(),
        "registry publication files must not exist"
    );

    let release = workflow("release.yml");
    for forbidden in [
        "publish_crates",
        "publish-crates.yml",
        "CRATES_IO_",
        "crates.io",
    ] {
        assert!(
            !release.contains(forbidden),
            "release.yml still contains registry publication surface {forbidden:?}"
        );
    }

    for path in [
        "README.md",
        "docs/index.md",
        "docs/zh/index.md",
        "docs/readme/README.zh-CN.md",
        "docs/guide/installation.md",
        "docs/zh/guide/installation.md",
        "docs/operate/release-pipeline.md",
        "docs/zh/operate/release-pipeline.md",
    ] {
        let text = std::fs::read_to_string(workspace_root().join(path))
            .unwrap_or_else(|error| panic!("read {path}: {error}"));
        assert!(
            !text.contains("cargo install zuno --locked") && !text.contains("crates.io"),
            "{path} still advertises registry publication"
        );
    }
}

#[test]
fn the_release_matrix_builds_every_target_the_project_ships() {
    let text = workflow("release-candidate.yml");
    let built = matrix_targets(&text, "artifact");
    let expected: BTreeSet<String> = RELEASE_TARGETS.iter().map(|t| (*t).to_owned()).collect();
    assert_eq!(
        built, expected,
        "release-candidate.yml's artifact matrix does not name exactly the six shipped targets"
    );
}

#[test]
fn windows_arm64_is_installed_and_updated_from_the_native_msvc_asset() {
    let installer =
        std::fs::read_to_string(workspace_root().join("scripts/install.ps1")).expect("installer");
    for required in [
        "\"AMD64\" { $Arch = \"x86_64\" }",
        "\"ARM64\" { $Arch = \"aarch64\" }",
        "$Target = \"$Arch-pc-windows-msvc\"",
    ] {
        assert!(
            installer.contains(required),
            "the Windows installer is missing architecture mapping {required:?}"
        );
    }

    let self_update =
        std::fs::read_to_string(workspace_root().join("crates/zuno-cli/src/cmd/self_update.rs"))
            .expect("self-update source");
    assert!(
        self_update.contains(
            "(\"windows\", \"aarch64\") => Ok(Self {\n                target: \
             \"aarch64-pc-windows-msvc\""
        ),
        "self-update does not select the Windows ARM64 release asset"
    );
}

#[test]
fn windows_installer_updates_only_the_user_path_without_setx() {
    let installer =
        std::fs::read_to_string(workspace_root().join("scripts/install.ps1")).expect("installer");
    assert!(
        !installer.to_ascii_lowercase().contains("setx"),
        "the Windows installer must not use setx because it can truncate PATH and \
         expands the current process's merged environment"
    );
    for required in [
        "[Environment]::GetEnvironmentVariable(",
        "[Environment]::SetEnvironmentVariable(",
        "[EnvironmentVariableTarget]::User",
        "$UpdatedUserPath = Add-PathEntry $UserPath $InstallDir",
        "$env:Path = Add-PathEntry $env:Path $InstallDir",
    ] {
        assert!(
            installer.contains(required),
            "the Windows installer is missing safe PATH behavior {required:?}"
        );
    }
}

#[test]
fn each_candidate_target_smokes_before_upload() {
    let text = workflow("release-candidate.yml");
    let jobs = job_names(&text);
    assert!(
        !jobs.contains("smoke"),
        "release candidate still has a global smoke job, recreating the matrix barrier"
    );

    let artifact = job_body(&text, "artifact").join("\n");
    let smoke = artifact
        .find("Smoke packaged artifact")
        .expect("artifact job contains its smoke steps");
    let attest = artifact
        .find("Attest packaged artifact")
        .expect("artifact job attests the smoked archive");
    let upload = artifact
        .find("Upload smoked target")
        .expect("artifact job uploads the certified bytes");
    assert!(
        smoke < attest && attest < upload,
        "the artifact job must smoke, then attest, then upload; positions were \
         smoke={smoke}, attest={attest}, upload={upload}"
    );
    for required in [
        "--binary unpacked/zuno",
        "--binary \"unpacked/zuno.exe\"",
        "name: candidate-${{ matrix.target }}",
        "retention-days: 7",
    ] {
        assert!(
            artifact.contains(required),
            "the per-target build/smoke/upload contract is missing {required:?}"
        );
    }
}

#[test]
fn release_binary_and_smoke_driver_share_one_cargo_invocation() {
    let text = workflow("release-candidate.yml");
    let artifact = job_body(&text, "artifact").join("\n");
    assert_eq!(
        artifact.matches("dtolnay/rust-toolchain@").count(),
        1,
        "each matrix leg must install its target with one Rust action"
    );
    for command in ["cargo zigbuild --locked", "cargo build --locked"] {
        let start = artifact
            .find(command)
            .unwrap_or_else(|| panic!("artifact job has no {command} invocation"));
        let tail = &artifact[start..];
        let end = tail.find("\n\n").unwrap_or(tail.len());
        let invocation = &tail[..end];
        for required in ["-p zuno --bin zuno", "-p zuno-testkit --bin zuno-smoke"] {
            assert!(
                invocation.contains(required),
                "{command} does not build both release binaries together:\n{invocation}"
            );
        }
    }
    for forbidden in ["Install MSVC build tools", "choco install visualstudio"] {
        assert!(
            !artifact.contains(forbidden),
            "GitHub's Windows image already carries MSVC; found {forbidden:?}"
        );
    }
}

#[test]
fn release_workflows_pin_the_verified_node24_artifact_actions() {
    const UPLOAD: &str =
        "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1";
    const DOWNLOAD: &str =
        "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1";

    let workflows = ["ci.yml", "release-candidate.yml", "release.yml"]
        .into_iter()
        .map(workflow)
        .collect::<Vec<_>>()
        .join("\n");
    let upload_count = workflows.matches("actions/upload-artifact@").count();
    let download_count = workflows.matches("actions/download-artifact@").count();
    assert_eq!(
        upload_count, 3,
        "the CI/release surface changed upload count"
    );
    assert_eq!(
        download_count, 2,
        "the CI/release surface changed download count"
    );
    assert_eq!(
        upload_count,
        workflows.matches(UPLOAD).count(),
        "every upload-artifact use in the CI/release surface must use the verified Node 24 pin"
    );
    assert_eq!(
        download_count,
        workflows.matches(DOWNLOAD).count(),
        "every download-artifact use in the CI/release surface must use the verified Node 24 pin"
    );
}

#[test]
fn release_pipeline_docs_keep_the_verified_twenty_minute_slo() {
    for (path, twenty, fifteen, identity, targets) in [
        (
            "docs/operate/release-pipeline.md",
            "within 20 minutes",
            "within 15 minutes",
            "candidate-byte identity",
            "six release targets",
        ),
        (
            "docs/zh/operate/release-pipeline.md",
            "20 分钟",
            "15 分钟",
            "候选字节身份",
            "六个发布目标",
        ),
    ] {
        let docs = std::fs::read_to_string(workspace_root().join(path))
            .unwrap_or_else(|error| panic!("read {path}: {error}"));
        for required in [twenty, "Rosetta 2", identity, targets] {
            assert!(
                docs.contains(required),
                "{path} lost the release timing or verification contract {required:?}"
            );
        }
        assert!(
            !docs.contains(fifteen),
            "{path} still advertises the obsolete 15-minute SLO"
        );
    }
}

#[test]
fn macos_x86_candidate_cross_builds_on_arm_and_smokes_through_rosetta() {
    let text = workflow("release-candidate.yml");
    let x86 = matrix_entry(&text, "artifact", "x86_64-apple-darwin").join("\n");
    for required in [
        "runner: macos-15",
        "cache_target: true",
        "execution_arch: x86_64",
    ] {
        assert!(
            x86.contains(required),
            "the x86_64 macOS candidate leg is missing {required:?}"
        );
    }
    assert!(
        !x86.contains("macos-15-intel"),
        "the macOS critical path still waits for the dedicated Intel runner"
    );

    let arm = matrix_entry(&text, "artifact", "aarch64-apple-darwin").join("\n");
    for required in [
        "runner: macos-15",
        "cache_target: true",
        "execution_arch: arm64",
    ] {
        assert!(
            arm.contains(required),
            "the arm64 macOS candidate leg is missing {required:?}"
        );
    }

    let artifact = job_body(&text, "artifact").join("\n");
    for required in [
        "EXECUTION_ARCH: ${{ matrix.execution_arch }}",
        "/usr/bin/lipo unpacked/zuno -verify_arch \"$EXECUTION_ARCH\"",
        "\"target/${{ matrix.target }}/release/zuno-smoke\" \\\n            -verify_arch \"$EXECUTION_ARCH\"",
        "/usr/bin/arch \"-${EXECUTION_ARCH}\"",
        "\"target/${{ matrix.target }}/release/zuno-smoke\"",
    ] {
        assert!(
            artifact.contains(required),
            "macOS packaging lost the exact-architecture execution proof {required:?}"
        );
    }
    assert!(
        !artifact.contains("/usr/bin/lipo -verify_arch"),
        "lipo requires each input file before the -verify_arch operation; putting the \
         operation first makes the file path parse as an architecture name"
    );
}

#[test]
fn public_workflows_use_only_standard_github_hosted_runners() {
    let workflows = [
        ("ci.yml", workflow("ci.yml")),
        ("release.yml", workflow("release.yml")),
        ("release-candidate.yml", workflow("release-candidate.yml")),
        ("publish-docs.yml", workflow("publish-docs.yml")),
    ];
    for (name, text) in &workflows {
        let code = text
            .lines()
            .map(|line| line.split('#').next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();
        for forbidden in ["codebuild-", "runs-on: self-hosted", "zuno-runner-"] {
            assert!(
                !code.contains(forbidden),
                "{name} still depends on {forbidden:?}"
            );
        }
    }

    let candidate = &workflows[2].1;
    for (target, runner) in [
        ("x86_64-unknown-linux-musl", "runner: ubuntu-24.04"),
        ("aarch64-unknown-linux-musl", "runner: ubuntu-24.04-arm"),
        ("x86_64-apple-darwin", "runner: macos-15"),
        ("aarch64-apple-darwin", "runner: macos-15"),
        ("x86_64-pc-windows-msvc", "runner: windows-2022"),
        ("aarch64-pc-windows-msvc", "runner: windows-11-arm"),
    ] {
        let entry = matrix_entry(candidate, "artifact", target).join("\n");
        assert!(
            entry.contains(runner),
            "{target} is not routed to its expected standard runner; missing {runner:?}"
        );
    }
}

#[test]
fn linux_ci_loads_the_reviewed_bubblewrap_profile_without_weakening_user_namespaces() {
    let setup =
        std::fs::read_to_string(workspace_root().join(".github/scripts/setup-linux-sandbox.sh"))
            .expect("sandbox setup script is readable");
    for required in [
        "apparmor-profiles",
        "bwrap-userns-restrict",
        "apparmor_parser -r",
        "--unshare-pid --unshare-uts --unshare-ipc",
        "--unshare-net",
    ] {
        assert!(
            setup.contains(required),
            "Linux CI sandbox setup is missing the reviewed deployment check {required:?}"
        );
    }
    for forbidden in [
        "apparmor_restrict_unprivileged_userns=0",
        "unprivileged_userns_clone=1",
        "chmod u+s",
        "setcap ",
    ] {
        assert!(
            !setup.contains(forbidden),
            "Linux CI sandbox setup weakens host policy with {forbidden:?}"
        );
    }

    for workflow_name in ["ci.yml", "release-candidate.yml"] {
        let text = workflow(workflow_name);
        assert!(
            text.contains(".github/scripts/setup-linux-sandbox.sh"),
            "{workflow_name} does not use the shared, probed Linux sandbox setup"
        );
        assert!(
            !text.contains("apparmor_restrict_unprivileged_userns=0"),
            "{workflow_name} disables Ubuntu's user-namespace restriction"
        );
    }
}

/// Installing bubblewrap proves nothing by itself. The only test that runs a real
/// Zuno process inside bwrap and checks the filesystem, network, capability, and
/// syscall boundaries needs host namespaces and a built executable, so it reports a
/// named skip when either is missing. The feature PR gate must therefore supply
/// them and demand real evidence, and the test must stay out of `#[ignore]`.
/// A release-only candidate proves its four-file delta before it skips this
/// duplicate full-suite gate; its Linux artifact legs still install the backend
/// and execute the packaged binary on the target host.
#[test]
fn feature_pr_executes_the_real_bubblewrap_boundary_before_release_candidates() {
    let makefile = std::fs::read_to_string(workspace_root().join("Makefile"))
        .expect("the workspace has a Makefile");
    let recipe: String = makefile
        .lines()
        .skip_while(|line| *line != "test-sandbox-e2e:")
        .skip(1)
        .take_while(|line| line.starts_with('\t'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !recipe.is_empty(),
        "the Makefile's `test-sandbox-e2e` target has no recipe"
    );
    for required in [
        "$(CARGO) build -p $(CLI_CRATE) --bin $(BINARY_NAME)",
        "ZUNO_SANDBOX_E2E_HELPER=",
        "$(CARGO) test -p zuno-sandbox --test linux_bubblewrap",
    ] {
        assert!(
            recipe.contains(required),
            "`make test-sandbox-e2e` no longer runs the confinement boundary test \
             through {required:?}:\n{recipe}"
        );
    }

    let boundary_test = std::fs::read_to_string(
        workspace_root().join("crates/zuno-sandbox/tests/linux_bubblewrap.rs"),
    )
    .expect("the bubblewrap boundary test is readable");
    assert!(
        !boundary_test.contains("#[ignore"),
        "the bubblewrap boundary test is ignored again, so no gate can execute it"
    );
    assert!(
        boundary_test.contains("ZUNO_SANDBOX_E2E_REQUIRE"),
        "the bubblewrap boundary test no longer distinguishes a skipped host from \
         a gate that demands real confinement evidence"
    );

    let ci = workflow("ci.yml");
    let gate = ci
        .find("make test-sandbox-e2e")
        .expect("ci.yml never runs `make test-sandbox-e2e`");
    let setup = ci
        .find(".github/scripts/setup-linux-sandbox.sh")
        .expect("ci.yml never installs the sandbox backend");
    assert!(
        setup < gate,
        "ci.yml runs the confinement boundary gate before installing bwrap"
    );
    assert!(
        ci.contains("ZUNO_SANDBOX_E2E_REQUIRE"),
        "ci.yml does not set ZUNO_SANDBOX_E2E_REQUIRE, so an unavailable bubblewrap \
         backend would be reported as a passing gate"
    );

    let candidate = workflow("release-candidate.yml");
    assert!(
        candidate.contains("release PR changed files outside the four-file release delta")
            && !candidate.contains("make test-sandbox-e2e"),
        "the candidate may skip the duplicate boundary suite only behind its release-only \
         delta proof"
    );
    let artifact = job_body(&candidate, "artifact").join("\n");
    let setup = artifact
        .find("name: Install Linux sandbox backend")
        .expect("candidate Linux artifacts never install the sandbox backend");
    let smoke = artifact
        .find("name: Smoke packaged artifact (Linux)")
        .expect("candidate Linux artifacts never execute the packaged binary");
    assert!(
        setup < smoke,
        "candidate Linux artifact smoke runs before installing the sandbox backend"
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

    let static_checks = job_body(&text, "linux-static").join("\n");
    for required in ["tool: cargo-deny", "cargo deny --all-features check"] {
        assert!(
            static_checks.contains(required),
            "the Linux static-check job lost the supply-chain gate {required:?}"
        );
    }
    let linux_tests = job_body(&text, "linux-test").join("\n");
    for required in ["make test-nextest", "make test-sandbox-e2e"] {
        assert!(
            linux_tests.contains(required),
            "the parallel Linux test job lost {required:?}"
        );
    }
    for forbidden in ["HOSTED_RUNNERS", "codebuild-"] {
        assert!(
            !text.contains(forbidden),
            "public-repository CI still carries the obsolete restriction {forbidden:?}"
        );
    }
    let classify = job_body(&text, "classify").join("\n");
    for required in [
        "PR_USER: ${{ github.event.pull_request.user.login }}",
        "HEAD_REPOSITORY: ${{ github.event.pull_request.head.repo.full_name }}",
        "PR_LABELS: ${{ toJSON(github.event.pull_request.labels.*.name) }}",
        "[ \"$PR_USER\" = 'github-actions[bot]' ]",
        "[ \"$HEAD_REPOSITORY\" = \"$GITHUB_REPOSITORY\" ]",
        "[[ \"$HEAD_REF\" == release-please--branches--main--* ]]",
        "index(\"autorelease: pending\") != null",
        "echo \"release_pr=${release_pr}\" >> \"$GITHUB_OUTPUT\"",
    ] {
        assert!(
            classify.contains(required),
            "release PR classification lost fail-closed identity check {required:?}"
        );
    }
    for forbidden in ["ACTOR:", "github.actor"] {
        assert!(
            !classify.contains(forbidden),
            "release PR classification treats the workflow initiator as commit identity via \
             {forbidden:?}"
        );
    }
    for job in [
        "linux-static",
        "linux-test",
        "artifact",
        "windows-clippy",
        "windows-test",
    ] {
        let body = job_body(&text, job).join("\n");
        for required in [
            "needs: classify",
            "if: needs.classify.outputs.release_pr != 'true'",
        ] {
            assert!(
                body.contains(required),
                "{job} does not delegate the exact release-please PR to candidate CI; \
                 missing {required:?}"
            );
        }
    }
    let gate = job_body(&text, "ci-success").join("\n");
    for required in [
        "Release PR routed to candidate",
        "needs: [classify, linux-static, linux-test, artifact, windows-clippy, windows-test]",
        "RELEASE_PR: ${{ needs.classify.outputs.release_pr }}",
        "elif $release_pr == \"true\" then",
        "release-please PR is delegated to release-candidate.yml",
    ] {
        assert!(
            gate.contains(required),
            "CI routing gate lost release-please handling {required:?}"
        );
    }
    for job in ["windows-clippy", "windows-test"] {
        assert!(
            job_body(&text, job)
                .join("\n")
                .contains("runs-on: windows-2022"),
            "the public repository's {job} gate is not always enabled"
        );
    }
}

#[test]
fn automated_release_prs_keep_the_manual_actions_approval_gate() {
    let root = workspace_root();
    let config_text = std::fs::read_to_string(root.join("release-please-config.json"))
        .expect("release-please config");
    let config: serde_json::Value =
        serde_json::from_str(&config_text).expect("valid release-please config");
    let title = config["pull-request-title-pattern"]
        .as_str()
        .expect("release PR title pattern");
    assert!(
        !title.to_ascii_lowercase().contains("skip"),
        "release-please must not bypass the manual Actions approval with a skip marker"
    );
    let header = config["pull-request-header"]
        .as_str()
        .expect("release PR approval instructions");
    for required in [
        "Manual release approval required",
        "exact head SHA",
        "keeps GitHub's native Actions approval gate",
        "`action_required`",
        "`zuno/pr-gate`",
        "merge manually",
    ] {
        assert!(
            header.contains(required),
            "release PR header lost manual approval instruction {required:?}"
        );
    }
    assert_eq!(
        config["packages"]["."]["bump-patch-for-minor-pre-major"],
        serde_json::json!(true),
        "pre-1.0 feature commits must remain patch releases"
    );
    for forbidden in ["[skip ci]", "[ci skip]", "pull_request_target"] {
        if forbidden == "pull_request_target" {
            assert!(
                header.contains("Do not bypass") && header.contains(forbidden),
                "release PR header must explicitly reject privileged {forbidden}"
            );
        } else {
            assert!(
                !config_text.to_ascii_lowercase().contains(forbidden),
                "release-please config bypasses approval with {forbidden}"
            );
        }
    }

    let release = workflow("release.yml");
    let dispatch = job_body(&release, "dispatch_candidate").join("\n");
    for required in [
        "name: Record required manual approval",
        "Manual release approval required",
        "If GitHub marks its ordinary CI run as \\`action_required\\`",
        "Approve its **CI** Actions run when GitHub requests approval",
        "Wait for the independently dispatched \\`zuno/pr-gate\\` candidate",
        "Do not replace this approval with a skip marker",
    ] {
        assert!(
            dispatch.contains(required),
            "release controller does not surface required operator action {required:?}"
        );
    }
    for required in [
        "name: Require patch-only rapid-development version",
        ".github/scripts/require-patch-release.py",
        "git show \"${HEAD_SHA}:.release-please-manifest.json\"",
    ] {
        assert!(
            dispatch.contains(required),
            "release candidate dispatch lost patch-only gate {required:?}"
        );
    }
    let resolve = job_body(&release, "resolve_release").join("\n");
    assert!(
        resolve.contains("require-patch-release.py \"${previous_tag#v}\" \"$version\""),
        "release publication can bypass the patch-only rapid-development gate"
    );
}

#[test]
fn release_controller_dispatches_exact_source_and_never_compiles() {
    let release = workflow("release.yml");
    let dispatch = job_body(&release, "dispatch_candidate").join("\n");
    for required in [
        "actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09",
        "fetch-depth: 0",
        "persist-credentials: false",
        ".github/scripts/resolve-release-pr-head.sh",
        "EXPECTED_BASE_SHA: ${{ github.sha }}",
        "RELEASE_PR_REFRESH_ENABLED: \"1\"",
        "RELEASE_PR_NUMBER=\"$number\"",
        "gh workflow run release-candidate.yml",
        "--ref \"$HEAD_REF\"",
        "-f expected_head_sha=\"$HEAD_SHA\"",
        "-f mode=automatic",
        "if [ \"${#numbers[@]}\" -eq 0 ]; then",
        "echo \"dispatch=false\" >> \"$GITHUB_OUTPUT\"",
        "if [ \"${#numbers[@]}\" -gt 1 ]; then",
        "if: steps.release_pr.outputs.dispatch == 'true'",
    ] {
        assert!(
            dispatch.contains(required),
            "release controller lost exact candidate dispatch field {required:?}"
        );
    }
    assert!(
        !dispatch.contains("if [ \"${#numbers[@]}\" -ne 1 ]; then"),
        "a routine main push without a release PR must be a successful no-op"
    );
    for forbidden in [
        "dtolnay/rust-toolchain@",
        "cargo build",
        "cargo zigbuild",
        "setup-zig",
        "Install MSVC",
    ] {
        assert!(
            !release.contains(forbidden),
            "release controller recompiles during promotion via {forbidden:?}"
        );
    }

    let resolver = std::fs::read_to_string(
        workspace_root().join(".github/scripts/resolve-release-pr-head.sh"),
    )
    .expect("read release PR resolver");
    for required in [
        "merge-base --is-ancestor",
        "cherry-pick \"$old_head\"",
        "--force-with-lease=\"refs/heads/${head_ref}:${old_head}\"",
        "GIT_ASKPASS",
        "previous_head_sha",
    ] {
        assert!(
            resolver.contains(required),
            "release PR refresh lost safety mechanism {required:?}"
        );
    }
    assert!(
        !resolver.contains("push --force "),
        "release PR refresh must never overwrite a concurrent branch update"
    );

    let resolve = job_body(&release, "resolve_release").join("\n");
    for required in [
        "id: release_input\n        shell: bash\n        env:\n          GH_TOKEN: ${{ github.token }}",
        "gh release view \"$TAG\"",
        "--json isDraft,tagName",
        ".context == \"zuno/pr-gate\"",
        "contents: write",
    ] {
        assert!(
            resolve.contains(required),
            "release recovery lost draft-aware identity check {required:?}"
        );
    }
    assert!(
        !resolve.contains("releases/tags/${TAG}"),
        "release recovery uses the REST by-tag endpoint that hides draft releases"
    );
}

#[test]
#[cfg(unix)]
fn release_pr_head_resolver_waits_for_a_fresh_stable_head() {
    use std::os::unix::fs::PermissionsExt;

    const OLD_BASE: &str = "1111111111111111111111111111111111111111";
    const OLD_HEAD: &str = "2222222222222222222222222222222222222222";
    const NEW_BASE: &str = "3333333333333333333333333333333333333333";
    const NEW_HEAD: &str = "4444444444444444444444444444444444444444";

    let fixture = tempfile::tempdir().expect("temporary resolver fixture");
    let old_pr = fixture.path().join("old-pr.json");
    let current_pr = fixture.path().join("current-pr.json");
    let commit = fixture.path().join("commit.json");
    let fake_gh = fixture.path().join("gh");
    let state = fixture.path().join("state");

    let pr = |base: &str, head: &str| {
        serde_json::json!({
            "state": "open",
            "user": {"login": "github-actions[bot]"},
            "base": {"ref": "main", "sha": base},
            "head": {
                "repo": {"full_name": "sunerpy/zuno"},
                "ref": "release-please--branches--main--components--zuno",
                "sha": head
            },
            "labels": [{"name": "autorelease: pending"}]
        })
    };
    std::fs::write(
        &old_pr,
        serde_json::to_vec(&pr(OLD_BASE, OLD_HEAD)).expect("encode old PR"),
    )
    .expect("write old PR");
    std::fs::write(
        &current_pr,
        serde_json::to_vec(&pr(NEW_BASE, NEW_HEAD)).expect("encode current PR"),
    )
    .expect("write current PR");
    std::fs::write(
        &commit,
        serde_json::to_vec(&serde_json::json!({
            "parents": [{"sha": NEW_BASE}],
            "author": {"login": "github-actions[bot]"},
            "commit": {
                "author": {
                    "email": "41898282+github-actions[bot]@users.noreply.github.com"
                },
                "message": "chore: release 0.4.0"
            }
        }))
        .expect("encode commit"),
    )
    .expect("write commit");

    let fake = r#"#!/usr/bin/env bash
set -euo pipefail
[ "$1" = api ]
case "$2" in
  */pulls/62)
    count=0
    if [ -f "$FAKE_STATE" ]; then
      read -r count < "$FAKE_STATE"
    fi
    count=$((count + 1))
    printf '%s\n' "$count" > "$FAKE_STATE"
    if [ "${FAKE_NEVER_STABLE:-0}" = 1 ] || [ "$count" -eq 1 ]; then
      exec cat "$FAKE_OLD_PR"
    fi
    exec cat "$FAKE_CURRENT_PR"
    ;;
  */commits/"$NEW_HEAD")
    exec cat "$FAKE_COMMIT"
    ;;
  *)
    echo "unexpected fake gh request: $*" >&2
    exit 1
    ;;
esac
"#;
    std::fs::write(&fake_gh, fake).expect("write fake gh");
    let mut permissions = std::fs::metadata(&fake_gh)
        .expect("fake gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, permissions).expect("make fake gh executable");

    let resolver = workspace_root().join(".github/scripts/resolve-release-pr-head.sh");
    let run = |state_path: &Path, never_stable: bool| {
        let mut command = Command::new("bash");
        command
            .arg(&resolver)
            .env("GITHUB_REPOSITORY", "sunerpy/zuno")
            .env("RELEASE_PR_NUMBER", "62")
            .env("EXPECTED_BASE_SHA", NEW_BASE)
            .env("RELEASE_PR_RESOLVE_ATTEMPTS", "3")
            .env("RELEASE_PR_RESOLVE_DELAY_SECONDS", "0")
            .env("GH_BIN", &fake_gh)
            .env("FAKE_STATE", state_path)
            .env("FAKE_OLD_PR", &old_pr)
            .env("FAKE_CURRENT_PR", &current_pr)
            .env("FAKE_COMMIT", &commit)
            .env("NEW_HEAD", NEW_HEAD);
        if never_stable {
            command.env("FAKE_NEVER_STABLE", "1");
        }
        command.output().expect("run release PR resolver")
    };

    let output = run(&state, false);
    assert!(
        output.status.success(),
        "resolver rejected a PR that became current:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resolved: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("resolver emits JSON");
    assert_eq!(resolved["number"], 62);
    assert_eq!(resolved["base_sha"], NEW_BASE);
    assert_eq!(resolved["head_sha"], NEW_HEAD);
    assert_eq!(resolved["refreshed"], false);

    let stale_state = fixture.path().join("stale-state");
    let stale = run(&stale_state, true);
    assert!(
        !stale.status.success(),
        "resolver accepted a PR that never reached the triggering main commit"
    );
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains("did not stabilize on main"),
        "stale failure did not explain the bounded wait:\n{}",
        String::from_utf8_lossy(&stale.stderr)
    );
}

#[test]
#[cfg(unix)]
fn release_pr_head_resolver_replays_one_trusted_commit_and_fails_closed_on_conflict() {
    use std::os::unix::fs::PermissionsExt;

    fn clear_parent_git_context(command: &mut Command) {
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_PREFIX",
        ] {
            command.env_remove(variable);
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        clear_parent_git_context(&mut command);
        let output = command
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git {} failed:\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    let fixture = tempfile::tempdir().expect("temporary resolver Git fixture");
    let remote = fixture.path().join("remote.git");
    let repository = fixture.path().join("repository");
    std::fs::create_dir(&remote).expect("create bare remote directory");
    std::fs::create_dir(&repository).expect("create fixture repository");
    git(&remote, &["init", "--bare"]);
    git(&repository, &["init", "-b", "main"]);
    git(
        &repository,
        &["config", "user.name", "Release Fixture Maintainer"],
    );
    git(
        &repository,
        &["config", "user.email", "maintainer@example.com"],
    );
    git(
        &repository,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("UTF-8 remote"),
        ],
    );

    std::fs::write(repository.join("version.txt"), "0.6.0\n").expect("write version fixture");
    std::fs::write(repository.join("guide.md"), "initial guide\n").expect("write guide fixture");
    git(&repository, &["add", "version.txt", "guide.md"]);
    git(&repository, &["commit", "-m", "feat: establish fixture"]);
    let old_base = git(&repository, &["rev-parse", "HEAD"]);
    git(&repository, &["push", "-u", "origin", "main"]);

    let head_ref = "release-please--branches--main--components--zuno";
    git(&repository, &["checkout", "-b", head_ref]);
    std::fs::write(repository.join("version.txt"), "0.6.1\n")
        .expect("write release version fixture");
    git(&repository, &["add", "version.txt"]);
    git(
        &repository,
        &[
            "-c",
            "user.name=github-actions[bot]",
            "-c",
            "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit",
            "-m",
            "chore: release 0.6.1",
        ],
    );
    let old_head = git(&repository, &["rev-parse", "HEAD"]);
    git(&repository, &["push", "-u", "origin", head_ref]);

    git(&repository, &["checkout", "main"]);
    std::fs::write(
        repository.join("guide.md"),
        "initial guide\ndocs-only clarification\n",
    )
    .expect("write docs-only main change");
    git(&repository, &["add", "guide.md"]);
    git(
        &repository,
        &["commit", "-m", "docs: clarify release guide"],
    );
    let expected_base = git(&repository, &["rev-parse", "HEAD"]);
    git(&repository, &["push", "origin", "main"]);
    assert_eq!(
        git(&repository, &["merge-base", &old_head, &expected_base]),
        old_base
    );

    let fake_gh = fixture.path().join("gh");
    let fake = r#"#!/usr/bin/env bash
set -euo pipefail
[ "$1" = api ]
case "$2" in
  */pulls/77)
    head=$(git --git-dir="$FAKE_REMOTE" rev-parse "refs/heads/$FAKE_HEAD_REF")
    jq -n \
      --arg repository "sunerpy/zuno" \
      --arg base "$FAKE_BASE_SHA" \
      --arg head "$head" \
      --arg head_ref "$FAKE_HEAD_REF" \
      '{
        state: "open",
        user: {login: "github-actions[bot]"},
        base: {ref: "main", sha: $base},
        head: {
          repo: {full_name: $repository},
          ref: $head_ref,
          sha: $head
        },
        labels: [{name: "autorelease: pending"}]
      }'
    ;;
  */commits/*)
    sha=${2##*/}
    parent=$(git --git-dir="$FAKE_REMOTE" show -s --format=%P "$sha")
    email=$(git --git-dir="$FAKE_REMOTE" show -s --format=%ae "$sha")
    message=$(git --git-dir="$FAKE_REMOTE" show -s --format=%B "$sha")
    jq -n \
      --arg parent "$parent" \
      --arg email "$email" \
      --arg message "$message" \
      '{
        parents: [{sha: $parent}],
        author: {login: "github-actions[bot]"},
        commit: {author: {email: $email}, message: $message}
      }'
    ;;
  *)
    echo "unexpected fake gh request: $*" >&2
    exit 1
    ;;
esac
"#;
    std::fs::write(&fake_gh, fake).expect("write dynamic fake gh");
    let mut permissions = std::fs::metadata(&fake_gh)
        .expect("dynamic fake gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, permissions).expect("make dynamic fake gh executable");

    let resolver = workspace_root().join(".github/scripts/resolve-release-pr-head.sh");
    let run = |base: &str| {
        let mut command = Command::new("bash");
        clear_parent_git_context(&mut command);
        command
            .arg(&resolver)
            .current_dir(&repository)
            .env("GITHUB_REPOSITORY", "sunerpy/zuno")
            .env("RELEASE_PR_NUMBER", "77")
            .env("EXPECTED_BASE_SHA", base)
            .env("RELEASE_PR_RESOLVE_ATTEMPTS", "5")
            .env("RELEASE_PR_RESOLVE_DELAY_SECONDS", "0")
            .env("RELEASE_PR_REFRESH_OBSERVATIONS", "1")
            .env("GH_TOKEN", "fixture-token")
            .env("GH_BIN", &fake_gh)
            .env("FAKE_REMOTE", &remote)
            .env("FAKE_HEAD_REF", head_ref)
            .env("FAKE_BASE_SHA", base)
            .output()
            .expect("run release PR resolver against Git fixture")
    };

    let refreshed = run(&expected_base);
    assert!(
        refreshed.status.success(),
        "resolver failed to refresh one trusted release commit:\n{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let resolved: serde_json::Value =
        serde_json::from_slice(&refreshed.stdout).expect("resolver emits refreshed JSON");
    assert_eq!(resolved["number"], 77);
    assert_eq!(resolved["base_sha"], expected_base);
    assert_eq!(resolved["refreshed"], true);
    assert_eq!(resolved["previous_base_sha"], old_base);
    assert_eq!(resolved["previous_head_sha"], old_head);

    let refreshed_head = git(&remote, &["rev-parse", &format!("refs/heads/{head_ref}")]);
    assert_ne!(refreshed_head, old_head);
    assert_eq!(
        git(&remote, &["show", "-s", "--format=%P", &refreshed_head]),
        expected_base
    );
    assert_eq!(
        git(&remote, &["show", "-s", "--format=%ae", &refreshed_head]),
        "41898282+github-actions[bot]@users.noreply.github.com"
    );
    assert_eq!(
        git(&remote, &["show", "-s", "--format=%s", &refreshed_head]),
        "chore: release 0.6.1"
    );

    std::fs::write(repository.join("version.txt"), "0.6.2-dev\n")
        .expect("write conflicting main version");
    git(&repository, &["add", "version.txt"]);
    git(
        &repository,
        &["commit", "-m", "docs: record development version"],
    );
    let conflicting_base = git(&repository, &["rev-parse", "HEAD"]);
    git(&repository, &["push", "origin", "main"]);

    let conflict = run(&conflicting_base);
    assert!(
        !conflict.status.success(),
        "resolver overwrote the release branch after a replay conflict"
    );
    assert!(
        String::from_utf8_lossy(&conflict.stderr).contains("CONFLICT"),
        "resolver did not expose the cherry-pick conflict:\n{}",
        String::from_utf8_lossy(&conflict.stderr)
    );
    assert_eq!(
        git(&remote, &["rev-parse", &format!("refs/heads/{head_ref}")]),
        refreshed_head,
        "a failed replay must leave the release PR branch unchanged"
    );
}

#[test]
fn candidate_auto_merge_is_separate_from_certification_and_wakes_finalization() {
    let text = workflow("release-candidate.yml");
    let prepare = job_body(&text, "prepare").join("\n");
    for required in [
        "state=pending",
        "context='zuno/pr-gate'",
        "actions/runs/${GITHUB_RUN_ID}",
    ] {
        assert!(
            prepare.contains(required),
            "candidate does not publish durable pending identity; missing {required:?}"
        );
    }

    let merge = job_body(&text, "merge").join("\n");
    assert!(
        text.contains(
            "if: inputs.mode == 'automatic' && vars.RELEASE_CANDIDATE_AUTO_MERGE == 'true'"
        ),
        "candidate certification must not imply automatic merge"
    );
    assert!(
        !merge.contains("RELEASE_CANDIDATE_AUTOMATION"),
        "the candidate dispatch switch must not authorize merging"
    );
    for required in [
        "--match-head-commit \"$EXPECTED_HEAD_SHA\"",
        "--auto",
        "gh workflow run release.yml",
        "--ref main",
        "-f candidate_run_id=\"$GITHUB_RUN_ID\"",
        "-f candidate_head_sha=\"$EXPECTED_HEAD_SHA\"",
    ] {
        assert!(
            merge.contains(required),
            "the GITHUB_TOKEN merge cannot wake finalization without {required:?}"
        );
    }
    for required in [
        "strict_required_status_checks_policy == true",
        ".context == \"zuno/pr-gate\"",
        "rules/branches/main",
    ] {
        assert!(
            merge.contains(required),
            "automatic merge does not fail closed on repository governance; missing {required:?}"
        );
    }
    assert!(
        !text.contains("context='zuno/release-candidate'"),
        "candidate status context differs from the protected zuno/pr-gate name"
    );
}

#[test]
fn publication_uses_one_exact_candidate_run() {
    let text = workflow("release.yml");
    let promote = job_body(&text, "promote").join("\n");
    for required in [
        "name: release-candidate",
        "run-id: ${{ needs.resolve_release.outputs.candidate_run_id }}",
        ".github/scripts/verify-release-candidate.sh",
        "gh attestation verify",
        "--signer-digest \"$SOURCE_SHA\"",
        "--source-digest \"$SOURCE_SHA\"",
        "--deny-self-hosted-runners",
        "gh release upload",
        "gh release edit \"$TAG\"",
    ] {
        assert!(
            promote.contains(required),
            "promotion is missing strict candidate check {required:?}"
        );
    }
    for forbidden in ["pattern:", "merge-multiple:", "latest artifact"] {
        assert!(
            !promote.contains(forbidden),
            "promotion can select an ambiguous candidate via {forbidden:?}"
        );
    }

    let resolve = job_body(&text, "resolve_release").join("\n");
    for required in [
        ".github/workflows/release-candidate.yml",
        ".conclusion",
        ".head_sha",
        ".run_attempt",
        "git rev-parse 'HEAD^{tree}'",
    ] {
        assert!(
            resolve.contains(required),
            "candidate run/source identity check is missing {required:?}"
        );
    }
}

#[test]
fn publication_includes_the_checksum_manifest_required_by_self_update() {
    let candidate = workflow("release-candidate.yml");
    let aggregate = job_body(&candidate, "aggregate").join("\n");
    for required in [
        ".github/scripts/assemble-release-candidate.sh",
        "name: release-candidate",
        "retention-days: 7",
    ] {
        assert!(
            aggregate.contains(required),
            "sealed candidate does not retain checksum input {required:?}"
        );
    }
    let release = workflow("release.yml");
    let promote = job_body(&release, "promote").join("\n");
    for required in ["candidate/SHA256SUMS", "SHA256SUMS"] {
        assert!(
            promote.contains(required),
            "published release omits checksum surface {required:?}"
        );
    }
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
    let text = workflow("release-candidate.yml");
    let build = job_body(&text, "artifact").join("\n");
    let mut offenders = Vec::new();
    for (index, line) in build.lines().enumerate() {
        let code = line.split('#').next().unwrap_or_default();
        let lowered = code.to_ascii_lowercase();
        let banned = [
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
                    "  release-candidate.yml artifact job, line {}: {what} (matched {needle:?})\n    {}",
                    index + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "release candidate reaches for a per-target C cross-toolchain. Zig plus \
         cargo-zigbuild is a hermetic C cross-compiler in one download and is the \
         only cross mechanism this pipeline may use:\n{}",
        offenders.join("\n")
    );

    // The positive half: the ruled-out mechanisms being absent proves nothing if
    // the sanctioned one is absent too.
    for required in [
        ".github/scripts/install-zig.sh",
        "cargo-zigbuild",
        "cargo zigbuild",
    ] {
        assert!(
            text.contains(required),
            "release-candidate.yml does not mention `{required}`; the two musl legs cannot \
             cross-compile this workspace's C dependencies without Zig"
        );
    }
    assert!(
        !text.contains("mlugg/setup-zig"),
        "release-candidate.yml reintroduced the Node-based setup-zig action"
    );

    let installer =
        std::fs::read_to_string(workspace_root().join(".github/scripts/install-zig.sh"))
            .expect("the pinned Zig installer is readable");
    for required in [
        "ZIG_VERSION=\"0.13.0\"",
        "zig-linux-${archive_arch}-${ZIG_VERSION}.tar.xz",
        "https://ziglang.org/download/${ZIG_VERSION}/${archive}",
        "d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea",
        "041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556",
        "sha256sum --check --strict -",
        "printf '%s\\n' \"$zig_dir\" >> \"$GITHUB_PATH\"",
    ] {
        assert!(
            installer.contains(required),
            "the Zig installer lost its pinned download contract {required:?}"
        );
    }
    for (musl, runner) in [
        ("x86_64-unknown-linux-musl", "runner: ubuntu-24.04"),
        ("aarch64-unknown-linux-musl", "runner: ubuntu-24.04-arm"),
    ] {
        let entry = matrix_entry(&text, "artifact", musl).join("\n");
        for required in ["use_zigbuild: true", runner] {
            assert!(
                entry.contains(required),
                "release-candidate.yml's `{musl}` entry is missing {required:?}"
            );
        }
    }
}

#[test]
fn candidate_manifest_records_the_identity_required_for_promotion() {
    let root = workspace_root();
    let assemble =
        std::fs::read_to_string(root.join(".github/scripts/assemble-release-candidate.sh"))
            .expect("read candidate assembler");
    let verify = std::fs::read_to_string(root.join(".github/scripts/verify-release-candidate.sh"))
        .expect("read candidate verifier");
    for required in [
        "schema_version",
        "repository",
        "workflow_ref",
        "workflow_sha",
        "run_id",
        "run_attempt",
        "release_pr_number",
        "head_sha",
        "release_pr_head_sha",
        "tree_sha",
        "version",
        "tag",
        "test_conclusion",
        "attestation_id",
        "smoke_conclusion",
    ] {
        assert!(
            assemble.contains(required),
            "candidate manifest does not record {required:?}"
        );
    }
    for required in [
        "EXPECTED_RUN_ID",
        "EXPECTED_RUN_ATTEMPT",
        "EXPECTED_PR_NUMBER",
        "EXPECTED_HEAD_SHA",
        "EXPECTED_PR_HEAD_SHA",
        "EXPECTED_TREE_SHA",
        ".workflow_sha == $head_sha",
        ".mode == \"automatic\" or .mode == \"backfill\"",
        "sha256sum --check --strict SHA256SUMS",
        "manifest target set is incomplete or duplicated",
    ] {
        assert!(
            verify.contains(required),
            "candidate verifier does not enforce {required:?}"
        );
    }
}

#[test]
#[cfg(unix)]
fn candidate_verifier_accepts_exact_bytes_and_rejects_tampering() {
    let workspace = workspace_root();
    let candidate = tempfile::tempdir().expect("temporary candidate");
    let evidence_dir = candidate.path().join("evidence");
    std::fs::create_dir(&evidence_dir).expect("create evidence directory");

    for target in RELEASE_TARGETS {
        let suffix = if target.contains("windows") {
            "zip"
        } else {
            "tar.gz"
        };
        let archive = format!("zuno-0.0.4-{target}.{suffix}");
        let bytes = format!("candidate bytes for {target}\n");
        std::fs::write(candidate.path().join(&archive), bytes.as_bytes())
            .expect("write candidate archive");
        let digest = Sha256::digest(bytes.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let evidence = serde_json::json!({
            "target": target,
            "archive": archive,
            "size": bytes.len(),
            "sha256": digest,
            "build_conclusion": "success",
            "smoke_conclusion": "success",
            "runner": "github-hosted",
            "attestation_id": format!("attestation-{target}"),
            "attestation_url": format!("https://example.invalid/{target}")
        });
        std::fs::write(
            evidence_dir.join(format!("{target}.json")),
            serde_json::to_vec_pretty(&evidence).expect("encode evidence"),
        )
        .expect("write target evidence");
    }

    let assemble = Command::new("bash")
        .arg(workspace.join(".github/scripts/assemble-release-candidate.sh"))
        .env("CANDIDATE_ROOT", candidate.path())
        .env("CANDIDATE_REPOSITORY", "sunerpy/zuno")
        .env(
            "CANDIDATE_WORKFLOW_REF",
            "sunerpy/zuno/.github/workflows/release-candidate.yml@refs/heads/release",
        )
        .env(
            "CANDIDATE_WORKFLOW_SHA",
            "2222222222222222222222222222222222222222",
        )
        .env("CANDIDATE_RUN_ID", "42")
        .env("CANDIDATE_RUN_ATTEMPT", "1")
        .env("CANDIDATE_PR_NUMBER", "7")
        .env("CANDIDATE_MODE", "automatic")
        .env(
            "CANDIDATE_HEAD_SHA",
            "2222222222222222222222222222222222222222",
        )
        .env(
            "CANDIDATE_PR_HEAD_SHA",
            "2222222222222222222222222222222222222222",
        )
        .env(
            "CANDIDATE_TREE_SHA",
            "3333333333333333333333333333333333333333",
        )
        .env("CANDIDATE_VERSION", "0.0.4")
        .status()
        .expect("run candidate assembler");
    assert!(
        assemble.success(),
        "candidate assembler rejected valid evidence"
    );

    let verify = || {
        Command::new("bash")
            .arg(workspace.join(".github/scripts/verify-release-candidate.sh"))
            .env("CANDIDATE_ROOT", candidate.path())
            .env("EXPECTED_REPOSITORY", "sunerpy/zuno")
            .env("EXPECTED_RUN_ID", "42")
            .env("EXPECTED_RUN_ATTEMPT", "1")
            .env("EXPECTED_PR_NUMBER", "7")
            .env(
                "EXPECTED_HEAD_SHA",
                "2222222222222222222222222222222222222222",
            )
            .env(
                "EXPECTED_PR_HEAD_SHA",
                "2222222222222222222222222222222222222222",
            )
            .env(
                "EXPECTED_TREE_SHA",
                "3333333333333333333333333333333333333333",
            )
            .env("EXPECTED_VERSION", "0.0.4")
            .env("EXPECTED_TAG", "v0.0.4")
            .status()
            .expect("run candidate verifier")
    };
    assert!(
        verify().success(),
        "candidate verifier rejected exact bytes"
    );

    let manifest_path = candidate.path().join("candidate-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read candidate manifest"))
            .expect("decode candidate manifest");
    manifest["mode"] = serde_json::Value::String("dry-run".to_owned());
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("encode dry-run manifest"),
    )
    .expect("write dry-run manifest");
    assert!(
        !verify().success(),
        "candidate verifier accepted a dry-run candidate for publication"
    );
    manifest["mode"] = serde_json::Value::String("automatic".to_owned());
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("restore automatic manifest"),
    )
    .expect("restore automatic manifest");

    std::fs::write(
        candidate
            .path()
            .join("zuno-0.0.4-x86_64-unknown-linux-musl.tar.gz"),
        b"tampered",
    )
    .expect("tamper with candidate");
    assert!(
        !verify().success(),
        "candidate verifier accepted bytes changed after sealing"
    );
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

/// A deadlocked test must fail with its own name.
///
/// Without a per-test ceiling a driver test that stops making progress blocks until the CI
/// job's `timeout-minutes`, which reports a job timeout and never names the test — the exact
/// failure this batch spent ten minutes diagnosing by hand. The ceiling lives in the default
/// nextest profile so it covers every binary, and the `#[ignore]`d soak, which is documented
/// to run for hours when explicitly requested, is exempted rather than killed.
#[test]
fn a_hung_test_is_terminated_with_its_own_name_instead_of_the_job_timeout() {
    let config_path = workspace_root().join(".config/nextest.toml");
    let config = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", config_path.display()));

    for required in [
        "[profile.default]",
        r#"slow-timeout = { period = "60s", terminate-after = 4 }"#,
        r#"filter = 'binary(soak)'"#,
        r#"slow-timeout = { period = "300s" }"#,
    ] {
        assert!(
            config.contains(required),
            "the nextest per-test ceiling lost {required:?}; a hung test would burn the job \
             timeout with no test name:\n{config}"
        );
    }
}

/// Startup telemetry is isolated so it remains useful, but ordinary hosted CI
/// must not turn shared-runner wall-clock variance into a product failure.
/// Stable hosts can opt into the absolute ceilings explicitly.
#[test]
fn startup_measurements_are_isolated_and_hosted_ci_is_observational() {
    let config_path = workspace_root().join(".config/nextest.toml");
    let config = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", config_path.display()));

    for required in [
        r#"nextest-version = "0.9.103""#,
        r#"filter = 'binary(startup)'"#,
        r#"threads-required = "num-test-threads""#,
    ] {
        assert!(
            config.contains(required),
            "nextest startup isolation lost {required:?}:\n{config}"
        );
    }

    let startup_path = workspace_root().join("crates/zuno-cli/tests/startup.rs");
    let startup = std::fs::read_to_string(&startup_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", startup_path.display()));
    assert!(
        startup.contains(
            "#[cfg(not(windows))]\nconst BUDGET_SESSION_LIST: Duration = Duration::from_millis(100);"
        ),
        "the stable-host Linux startup target changed without new measurements"
    );
    for required in [
        r#"const ENFORCE_STARTUP_BUDGET_ENV: &str = "ZUNO_ENFORCE_STARTUP_BUDGET";"#,
        "fn enforce_startup_budget() -> bool",
        "startup_medians_are_reported_and_stable_host_budgets_are_optional",
        "observational budget exceedance (not a hosted-CI failure)",
    ] {
        assert!(
            startup.contains(required),
            "startup measurement lost its explicit stable-host boundary {required:?}"
        );
    }
    assert!(
        !startup.contains(r#"measure("watchdog-cost""#),
        "the watchdog-active session-list path is already covered by the primary startup \
         budget; a second wall-clock measurement recreates the hosted-runner flake without \
         measuring incremental watchdog cost"
    );
    let scheduler_path = workspace_root().join("scripts/test-parallel.sh");
    let scheduler = std::fs::read_to_string(&scheduler_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", scheduler_path.display()));
    for required in [
        "isolated_suites = {'startup'}",
        "def publish_startup_measurement(index):",
        "shutil.copyfile(source, stable)",
        "GITHUB_STEP_SUMMARY",
        "ZUNO_ENFORCE_STARTUP_BUDGET=1",
        "if target == 'startup':",
        "arguments.append('--nocapture')",
    ] {
        assert!(
            scheduler.contains(required),
            "the startup telemetry path lost {required:?}"
        );
    }
    for forbidden in [
        "is_windows_startup_budget_only_failure",
        "WINDOWS STARTUP POST-LINK FIRST PROCESS",
        "WINDOWS STARTUP FRESH-PROCESS CONFIRMATION",
        "retry_code, retry_output, retry_elapsed",
    ] {
        assert!(
            !scheduler.contains(forbidden),
            "hosted CI still retries a wall-clock observation via {forbidden:?}"
        );
    }
}

/// The shared gate targets the plan names, plus the ones CI invokes by name. A workflow
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
        // The plan's shared gates.
        "fmt",
        "lint",
        "lint-windows-cross",
        "test",
        "test-nextest",
        "test-par",
        "ci",
        "pre-ci",
        "release",
        // Invoked by name from .github/workflows/ci.yml.
        "fmt-check",
        "test-sandbox-e2e",
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
    for prerequisite in ["fmt-check", "lint", "test-par", "deny"] {
        assert!(
            ci_line.contains(prerequisite),
            "`make ci` does not run `{prerequisite}` ({ci_line}); the local gate \
            and the CI gate would then check different things"
        );
    }

    let pre_ci_line = text
        .lines()
        .find(|line| line.starts_with("pre-ci:"))
        .expect("the Makefile declares a `pre-ci` target");
    for prerequisite in [
        "ci",
        "check",
        "test-sandbox-e2e",
        "smoke-artifact",
        "lint-windows-cross",
    ] {
        assert!(
            pre_ci_line.contains(prerequisite),
            "`make pre-ci` does not run `{prerequisite}` ({pre_ci_line}); avoidable \
             artifact or Windows failures would still be discovered only by hosted CI"
        );
    }

    let test_recipe = text
        .lines()
        .skip_while(|line| *line != "test:")
        .nth(1)
        .expect("the `test` target has a recipe");
    assert!(
        test_recipe.contains("cargo test --workspace --no-fail-fast")
            || test_recipe.contains("$(CARGO) test --workspace --no-fail-fast"),
        "`make test` must report failures from every test binary in one run ({test_recipe})"
    );

    for required in [
        "test-nextest:",
        "$(CARGO) nextest run --workspace --no-fail-fast --no-tests=warn",
        "$(CARGO) test --workspace --doc --no-fail-fast",
        "cargo-nextest unavailable; using scripts/test-parallel.sh",
    ] {
        assert!(
            text.contains(required),
            "the concurrent workspace gate lost {required:?}"
        );
    }

    let ci = workflow("ci.yml");
    let windows = job_body(&ci, "windows-test").join("\n");
    for required in [
        "run: ./scripts/test-parallel.sh",
        "timeout-minutes: 20",
        "CARGO_PROFILE_TEST_DEBUG: \"0\"",
        "CARGO_PROFILE_TEST_SPLIT_DEBUGINFO: \"off\"",
        "JOBS: \"4\"",
        "RUN_DOCTESTS: \"0\"",
        "THREADS: \"1\"",
        "SUITE_TIMEOUT: \"300\"",
        "name: Upload Windows test diagnostics",
        "if: failure()",
        "target/test-parallel/artifacts.json",
        "target/test-parallel/suites.tsv",
        "target/test-parallel/codes.tsv",
        "target/test-parallel/logs/",
    ] {
        assert!(
            windows.contains(required),
            "native Windows CI lost the bounded binary-parallel full-suite contract \
             {required:?}"
        );
    }
    assert!(
        !windows.contains("cargo nextest"),
        "native Windows CI must not spawn one process per test case; use the binary-level \
         scheduler instead"
    );
    assert!(
        !windows.contains("target/test-parallel/cargo-env.json"),
        "the private captured Cargo environment must never be uploaded"
    );
    assert!(
        !ci.contains("cargo test --workspace --no-fail-fast"),
        "hosted CI regressed to Cargo's serial test-binary execution"
    );

    let scheduler = std::fs::read_to_string(workspace_root().join("scripts/test-parallel.sh"))
        .expect("the binary-parallel test scheduler is readable");
    for required in [
        "RUN_DOCTESTS=${RUN_DOCTESTS:-1}",
        "export PYTHONUTF8=${PYTHONUTF8:-1}",
        "export PYTHONIOENCODING=${PYTHONIOENCODING:-utf-8:backslashreplace}",
        "--no-run",
        "--timings",
        "--message-format=json",
        r#"2> >(tee "$WORK/build.log" >&2)"#,
        "ThreadPoolExecutor",
        "as_completed",
        "SUITE_TIMEOUT=${SUITE_TIMEOUT:-300}",
        r#""$candidate" -c 'import json, os, sys'"#,
        r#"PYTHON=$(command -v "$candidate")"#,
        "runner_python_for_cargo",
        "test-parallel-duration-hints.json",
        "suite_key",
        "known.get(r[3]",
        "Cargo runner path must not contain whitespace",
        "cargo-env.json",
        "json.dump(dict(os.environ)",
        "shutil.which(\"rg\"",
        "[rg, \"--version\"]",
        "cygpath -m",
        "taskkill",
        "os.killpg",
        "isolated_suites = {'startup'}",
        "running isolated timing suite",
        "parallel_rows = [",
        "ThreadPoolExecutor(max_workers=jobs)",
        "futures = [pool.submit(run, indexed) for indexed in parallel_rows]",
        "f'--test-threads={threads}'",
        r#"if [[ "$RUN_DOCTESTS" == "1" ]]"#,
        "--doc",
        "skipping doctests explicitly",
    ] {
        assert!(
            scheduler.contains(required),
            "the binary-parallel scheduler lost {required:?}"
        );
    }
    for forbidden in [
        "exclusive_suites",
        "running exclusive suite",
        "suite_threads",
    ] {
        assert!(
            !scheduler.contains(forbidden),
            "the Windows scheduler regressed to the old serial-tail implementation: \
             {forbidden:?}"
        );
    }
    for forbidden in ["'acp_stdio'", "'windows_lifecycle'"] {
        assert!(
            !scheduler.contains(forbidden),
            "functional native-Windows suite {forbidden:?} must remain in the bounded \
             worker pool; only the startup timing benchmark is isolated"
        );
    }
    for forbidden in ["dumpenv.sh", "cargo-env.txt"] {
        assert!(
            !scheduler.contains(forbidden),
            "the scheduler regressed to the Git Bash environment format that corrupts \
             native Windows process environments: {forbidden:?}"
        );
    }
}

#[test]
fn ci_uses_target_isolated_dependency_caches_on_the_measured_critical_paths() {
    for name in ["ci.yml", "release-candidate.yml"] {
        let workflow = workflow(name);
        for required in [
            "CARGO_INCREMENTAL: 0",
            "SCCACHE_GHA_ENABLED: \"true\"",
            "RUSTC_WRAPPER: sccache",
            "mozilla-actions/sccache-action@fc920bf0ec8de6ee65d409111f7ec508035751ba",
            "version: \"v0.16.0\"",
        ] {
            assert!(
                workflow.contains(required),
                "{name} lost the compiler-cache contract {required:?}"
            );
        }
        assert!(
            !workflow.contains("tool: sccache")
                && !workflow.contains(".github/scripts/setup-sccache.sh")
                && !workflow.contains("sccache --show-stats"),
            "{name} must use the official cache action's setup and post-run statistics"
        );
    }

    let ci = workflow("ci.yml");
    for job in ["linux-static", "windows-clippy"] {
        let body = job_body(&ci, job).join("\n");
        assert!(
            body.contains("cache-targets: false"),
            "{job} must keep registry-only caching because it is not on the measured critical path"
        );
    }
    for (job, key) in [
        (
            "linux-test",
            "shared-key: pr-linux-tests-v1-${{ runner.os }}-${{ runner.arch }}",
        ),
        (
            "artifact",
            "shared-key: pr-host-release-v1-${{ runner.os }}-${{ runner.arch }}",
        ),
        (
            "windows-test",
            "shared-key: pr-windows-tests-v1-${{ runner.os }}-${{ runner.arch }}",
        ),
    ] {
        let body = job_body(&ci, job).join("\n");
        for required in [key, "cache-targets: true", "cache-workspace-crates: false"] {
            assert!(
                body.contains(required),
                "{job} lost its target-isolated dependency cache contract {required:?}"
            );
        }
    }
    let linux_static = job_body(&ci, "linux-static").join("\n");
    let linux_test = job_body(&ci, "linux-test").join("\n");
    assert!(
        linux_static.contains("make lint")
            && !linux_static.contains("make test-nextest")
            && linux_test.contains("make test-nextest")
            && linux_test.contains("make test-sandbox-e2e")
            && !linux_test.contains("make lint"),
        "Linux static analysis and tests must remain independent parallel jobs"
    );

    let windows_clippy = job_body(&ci, "windows-clippy").join("\n");
    let windows_test = job_body(&ci, "windows-test").join("\n");
    let clippy_fetch = windows_clippy.find("cargo fetch --locked");
    let clippy_run =
        windows_clippy.find("cargo clippy --locked --workspace --all-targets -- -D warnings");
    assert!(
        clippy_fetch
            .zip(clippy_run)
            .is_some_and(|(fetch, clippy)| fetch < clippy)
            && !windows_clippy.contains("test-parallel.sh"),
        "Windows Clippy must populate its cold Cargo cache and remain an independent \
         parallel job"
    );
    let test_fetch = windows_test.find("cargo fetch --locked");
    let test_run = windows_test.find("run: ./scripts/test-parallel.sh");
    assert!(
        test_fetch
            .zip(test_run)
            .is_some_and(|(fetch, test)| fetch < test)
            && !windows_test.contains("cargo clippy")
            && windows_test.contains("tool: ripgrep")
            && windows_test.contains("run: rg --version"),
        "Windows test execution must populate its cold Cargo cache, install its required \
         runtime dependency, and remain an independent parallel job"
    );
    assert!(
        windows_test.contains("RUN_DOCTESTS: \"0\"")
            && windows_test.contains("CARGO_PROFILE_TEST_DEBUG: \"0\"")
            && windows_test.contains("CARGO_PROFILE_TEST_SPLIT_DEBUGINFO: \"off\"")
            && !windows_test.contains("cargo test --workspace --doc"),
        "Windows must retain Cargo's built-in test-directory contract while reducing debug \
         link work, and the Linux source gate must own the doctest surface exactly once"
    );

    let candidate = workflow("release-candidate.yml");
    let prepare = job_body(&candidate, "prepare").join("\n");
    for required in [
        "release PR head must have its exact base as its single parent",
        "release PR changed files outside the four-file release delta",
        ".release-please-manifest.json\\nCHANGELOG.md\\nCargo.lock\\nCargo.toml",
        "git diff --check",
        ".github/scripts/require-patch-release.py",
    ] {
        assert!(
            prepare.contains(required),
            "candidate identity validation lost the release-only delta guard {required:?}"
        );
    }

    let tests = job_body(&candidate, "test").join("\n");
    for required in [
        "name: Candidate release delta",
        "cargo metadata --locked --format-version 1",
        "cargo deny --all-features check",
        "shared-key: candidate-release-delta-${{ runner.os }}-${{ runner.arch }}",
        "cache-targets: false",
        "SCCACHE_GHA_ENABLED: \"false\"",
        "RUSTC_WRAPPER: \"\"",
    ] {
        assert!(
            tests.contains(required),
            "candidate release-delta verification lost {required:?}"
        );
    }
    for forbidden in [
        "make lint",
        "make test-nextest",
        "make test-sandbox-e2e",
        "nextest@",
        "setup-linux-sandbox.sh",
        "sccache-action@",
    ] {
        assert!(
            !tests.contains(forbidden),
            "release-only candidate verification must not repeat the feature PR gate: \
             {forbidden:?}"
        );
    }

    let artifacts = job_body(&candidate, "artifact").join("\n");
    for required in [
        "shared-key: candidate-${{ matrix.target }}",
        "cache-targets: ${{ matrix.cache_target }}",
        "cache-workspace-crates: false",
    ] {
        assert!(
            artifacts.contains(required),
            "candidate artifact caching lost its target-isolated dependency contract \
             {required:?}"
        );
    }
    for target in [
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    ] {
        assert!(
            matrix_entry(&candidate, "artifact", target)
                .join("\n")
                .contains("cache_target: true"),
            "{target} must retain its measured dependency target cache"
        );
    }
    for target in ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"] {
        assert!(
            matrix_entry(&candidate, "artifact", target)
                .join("\n")
                .contains("cache_target: false"),
            "{target} must not upload a large Cargo target cache"
        );
    }
}

#[test]
fn local_pre_ci_catches_windows_cfg_failures_without_a_second_runner_service() {
    let script =
        std::fs::read_to_string(workspace_root().join(".github/scripts/lint-windows-cross.sh"))
            .expect("the Windows cross-Clippy preflight is readable");

    for required in [
        "x86_64-pc-windows-gnu",
        "zig cc -target x86_64-windows-gnu",
        "zig dlltool",
        "cargo-zigbuild",
        "zigbuild",
        "clippy",
        "--workspace",
        "--all-targets",
        "--tests",
        "-D warnings",
        "AWS_LC_SYS_NO_JITTER_ENTROPY=1",
    ] {
        assert!(
            script.contains(required),
            "the local Windows preflight lost {required:?}"
        );
    }
    for forbidden in ["docker ", "codebuild", "aws "] {
        assert!(
            !script.to_ascii_lowercase().contains(forbidden),
            "the local Windows preflight reintroduced an external runner dependency via \
             {forbidden:?}"
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

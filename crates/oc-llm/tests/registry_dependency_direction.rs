//! Asserts the dependency edge this registry exists to keep absent.
//!
//! `oc-llm` must not depend on any `oc-provider-*` crate, directly or through any
//! other first-party crate. That is not a style preference: `oc-llm` is upstream of
//! the engine, the server, the renderer and the CLI, so the edge would make a
//! one-line edit to a wire protocol recompile all of them.
//!
//! The acceptance criterion for this work is `cargo tree -p oc-llm | grep
//! oc-provider-` returning nothing. This test is the durable form of that command:
//! it needs no cargo subprocess, cannot be skipped by a sandbox without a
//! toolchain, and runs on every `cargo test`. Five provider crates are still to be
//! written (todos 29, 30, 94, 95, 96) and any of them could re-add the edge — this
//! is what stops that happening quietly.
//!
//! The scan is textual, matching the precedent set by
//! `crates/oc-error/tests/no_anyhow_in_libraries.rs`: it reports a violation in a
//! crate that does not yet compile, which is exactly when the report is most
//! useful.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// The crate whose dependency closure must stay free of provider implementations.
const SPINE: &str = "oc-llm";

/// The prefix no crate in the spine's closure may carry.
const FORBIDDEN_PREFIX: &str = "oc-provider-";

/// Floors, not exact counts, for the same reason `no_anyhow_in_libraries.rs` uses
/// them: a scanner that silently walked the wrong directory must fail loudly
/// rather than pass vacuously.
const MINIMUM_MEMBERS: usize = 33;

/// The five provider families the workspace roster reserves. Named here so this
/// test fails if the roster is renamed out from under it, rather than passing
/// because it found nothing to forbid.
const PROVIDER_CRATES: &[&str] = &[
    "oc-provider-anthropic",
    "oc-provider-bedrock",
    "oc-provider-compatible",
    "oc-provider-google",
    "oc-provider-openai",
];

fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR is always <root>/crates/oc-llm")
        .to_path_buf()
}

/// Drops `#` comments so prose *about* the ban — the note in `oc-llm/Cargo.toml`
/// explaining why the edge is absent — does not read as a dependency line.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(at) => &line[..at],
        None => line,
    }
}

/// Every first-party dependency declared by one manifest, across all dependency
/// kinds.
///
/// `dev-dependencies` and `build-dependencies` are included deliberately. A dev
/// dependency on a provider crate would still put the provider in the spine's
/// compile graph whenever the spine's tests build, which is every CI run, so it
/// costs the same rebuild the normal edge would.
fn first_party_dependencies(manifest: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let mut in_dependency_table = false;

    for raw in manifest.lines() {
        let line = strip_comment(raw).trim();
        if line.starts_with('[') {
            // Covers `[dependencies]`, `[dev-dependencies]`,
            // `[build-dependencies]` and any `[target.'cfg(..)'.dependencies]`.
            in_dependency_table = line.ends_with("dependencies]");
            continue;
        }
        if !in_dependency_table {
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let name = key.trim().trim_matches('"');
        if name.starts_with("oc-") {
            deps.insert(name.to_owned());
        }
    }

    deps
}

/// The first-party dependency graph of every workspace member.
fn dependency_graph() -> BTreeMap<String, BTreeSet<String>> {
    let crates_dir = workspace_root().join("crates");
    let entries = std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", crates_dir.display()));

    let mut graph = BTreeMap::new();
    for entry in entries {
        let path = entry.expect("directory entry").path();
        let manifest_path = path.join("Cargo.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .expect("crate directory has a name")
            .to_string_lossy()
            .into_owned();
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest_path.display()));
        graph.insert(name, first_party_dependencies(&manifest));
    }
    graph
}

/// Breadth-first closure of `root`, so an edge added through an intermediate crate
/// is caught as readily as a direct one.
fn transitive_closure(graph: &BTreeMap<String, BTreeSet<String>>, root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(root.to_owned());

    while let Some(current) = queue.pop_front() {
        for dependency in graph.get(&current).into_iter().flatten() {
            if seen.insert(dependency.clone()) {
                queue.push_back(dependency.clone());
            }
        }
    }
    seen
}

#[test]
fn registry_guard_actually_reads_the_workspace() {
    let graph = dependency_graph();
    assert!(
        graph.len() >= MINIMUM_MEMBERS,
        "found only {} workspace members under crates/; the scan is looking in the wrong place \
         and every other assertion in this file would pass vacuously",
        graph.len()
    );
    assert!(
        graph.contains_key(SPINE),
        "{SPINE} is not among the workspace members this scan found"
    );
    for provider in PROVIDER_CRATES {
        assert!(
            graph.contains_key(*provider),
            "{provider} is not a workspace member; either the roster was renamed or this test \
             is forbidding a prefix nothing uses"
        );
    }
}

#[test]
fn registry_guard_parser_recognizes_a_dependency_it_should_find() {
    let graph = dependency_graph();
    let spine = graph
        .get(SPINE)
        .expect("oc-llm is a workspace member")
        .clone();
    assert!(
        spine.contains("oc-error"),
        "the manifest parser did not find oc-llm's dependency on oc-error, so its failure to \
         find an oc-provider-* dependency proves nothing; parsed: {spine:?}"
    );
}

#[test]
fn registry_spine_does_not_depend_on_any_provider_crate_directly() {
    let graph = dependency_graph();
    let direct = graph.get(SPINE).expect("oc-llm is a workspace member");
    let offenders: Vec<&String> = direct
        .iter()
        .filter(|dep| dep.starts_with(FORBIDDEN_PREFIX))
        .collect();

    assert!(
        offenders.is_empty(),
        "{SPINE}/Cargo.toml declares {offenders:?}. {SPINE} is the spine every provider family \
         implements against; naming a family here makes a wire-protocol edit recompile the \
         engine, the server, the renderer and the CLI. Register the provider from oc-cli's \
         composition root instead — see crates/oc-llm/src/registry.rs."
    );
}

#[test]
fn registry_spine_reaches_no_provider_crate_through_an_intermediate() {
    let graph = dependency_graph();
    let closure = transitive_closure(&graph, SPINE);
    let offenders: Vec<&String> = closure
        .iter()
        .filter(|dep| dep.starts_with(FORBIDDEN_PREFIX))
        .collect();

    assert!(
        offenders.is_empty(),
        "{SPINE}'s transitive first-party closure contains {offenders:?}. The edge is indirect, \
         but it costs the same rebuild as a direct one. Full closure: {closure:?}"
    );
}

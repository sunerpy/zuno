//! The structural guard behind "the harness never makes a live provider call".
//!
//! An invariant that lives in a comment is a suggestion. This crate instead has no
//! HTTP client in its dependency graph, so a live call is not something a later
//! task has to remember not to write — it is something it cannot write without
//! first adding a dependency, which fails here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Crates that can originate an outbound HTTP request.
///
/// `axum`, `tower` and `hyper`'s server half are absent from this list on purpose:
/// a server cannot make a request. `hyper` is listed because its client is in the
/// same crate.
const HTTP_CLIENTS: &[&str] = &[
    "reqwest",
    "hyper",
    "ureq",
    "curl",
    "isahc",
    "surf",
    "attohttpc",
    "hyper-util",
];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn declared_dependencies() -> BTreeSet<String> {
    let path = manifest_dir().join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let manifest: toml::Value =
        toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    let mut names = BTreeSet::new();
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(toml::Value::Table(entries)) = manifest.get(table) {
            names.extend(entries.keys().cloned());
        }
    }
    names
}

#[test]
fn the_harness_has_no_http_client_and_therefore_cannot_make_a_live_call() {
    let declared = declared_dependencies();
    let offenders: Vec<&String> = declared
        .iter()
        .filter(|name| HTTP_CLIENTS.contains(&name.as_str()))
        .collect();
    assert!(
        offenders.is_empty(),
        "zuno-testkit declared an HTTP client: {offenders:?}.\n\
         The no-live-provider-call invariant is enforced by the absence of this capability. \
         If a consumer needs a client, it brings its own and points it at \
         MockProvider::base_url(), which is always loopback."
    );
}

/// The server side is present and pinned from the workspace, so the mock provider
/// really is a server rather than something improvised over a client.
#[test]
fn the_harness_has_the_server_it_needs() {
    let declared = declared_dependencies();
    for required in ["axum", "tokio"] {
        assert!(
            declared.contains(required),
            "zuno-testkit needs {required} to run a loopback provider stand-in"
        );
    }
}

/// Nothing in this crate names a remote endpoint outside a doc comment. Provider
/// hostnames belong in the recorded cassettes, which are read from the oracle
/// tree, never compiled in.
#[test]
fn no_source_line_targets_a_remote_endpoint() {
    let src = manifest_dir().join("src");
    let mut offences = Vec::new();
    for entry in walkdir::WalkDir::new(&src)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
    {
        scan(entry.path(), &mut offences);
    }
    assert!(
        offences.is_empty(),
        "these lines name a remote endpoint in executable code:\n{}",
        offences.join("\n")
    );
}

fn scan(path: &Path, offences: &mut Vec<String>) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut in_test_module = false;
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("mod tests") {
            in_test_module = true;
        }
        // Doc comments describe the format and cite real URLs; tests assert on the
        // recorded ones. Neither can perform a request, since there is no client.
        if trimmed.starts_with("//") || in_test_module {
            continue;
        }
        if trimmed.contains("https://") || trimmed.contains("http://") {
            let allowed_loopback =
                trimmed.contains("http://127.0.0.1") || trimmed.contains("http://{addr}");
            if !allowed_loopback {
                offences.push(format!("{}:{}: {trimmed}", path.display(), number + 1));
            }
        }
    }
}

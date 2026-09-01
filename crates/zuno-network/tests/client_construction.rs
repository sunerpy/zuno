//! Architectural guard for process-wide proxy inheritance.
//!
//! A provider that constructs reqwest directly can accidentally opt out of the
//! session networking contract or grow a second interpretation of proxy policy.
//! All production client construction therefore belongs in `zuno-network`.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("zuno-network lives at <workspace>/crates/zuno-network")
        .to_path_buf()
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(root).unwrap_or_else(|error| panic!("read {}: {error}", root.display()));
    for entry in entries {
        let path = entry.expect("source directory entry").path();
        if path.is_dir() {
            rust_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
}

#[test]
fn production_crates_construct_reqwest_only_through_zuno_network() {
    let crates = workspace_root().join("crates");
    let mut sources = Vec::new();
    let mut crate_count = 0usize;
    for entry in std::fs::read_dir(&crates).expect("workspace crates directory") {
        let crate_root = entry.expect("crate directory entry").path();
        if !crate_root.join("Cargo.toml").is_file()
            || crate_root
                .file_name()
                .is_some_and(|name| name == "zuno-network")
        {
            continue;
        }
        crate_count += 1;
        let src = crate_root.join("src");
        if src.is_dir() {
            rust_sources(&src, &mut sources);
        }
    }
    assert!(
        crate_count >= 40 && sources.len() >= 300,
        "network construction guard scanned an implausibly small workspace: \
         {crate_count} crates, {} Rust sources",
        sources.len()
    );

    let mut offenders = Vec::new();
    for source in &sources {
        let text = std::fs::read_to_string(source)
            .unwrap_or_else(|error| panic!("read {}: {error}", source.display()));
        let imports_reqwest_client = text.lines().any(|line| {
            let line = line.trim();
            line.starts_with("use reqwest::Client")
                || (line.starts_with("use reqwest::{") && line.contains("Client"))
        });
        for (offset, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            let fully_qualified = code.contains("reqwest::Client::builder(")
                || code.contains("reqwest::Client::new(");
            let imported = imports_reqwest_client
                && (code.contains("Client::builder(") || code.contains("Client::new("));
            if fully_qualified || imported {
                offenders.push(format!("{}:{}: {code}", source.display(), offset + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "production code constructed reqwest outside zuno-network:\n{}\n\
         Use zuno_network::client_builder/client for ordinary session traffic, or \
         direct_client_builder with an explicit DirectPurpose.",
        offenders.join("\n")
    );
}

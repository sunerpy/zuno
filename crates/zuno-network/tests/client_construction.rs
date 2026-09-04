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

/// Every reqwest client construction in one file, paired with its line number.
///
/// `Client` is a common type name, so a bare `Client::new()` only counts in a file that
/// pulled the type in from reqwest. The import scan therefore has to understand the same
/// forms rustc does - grouped, multi-line, glob, and renamed - because each of those is a
/// way to reach `reqwest::Client` under a name this scan would otherwise not recognize.
///
/// Known residual: a construction that reaches reqwest through another crate's re-export,
/// or through a type alias declared elsewhere in the same crate, is not detected. That is
/// a deliberate limit of a textual guard, not a claim about every possible form.
fn reqwest_constructions(text: &str) -> Vec<(usize, String)> {
    const CONSTRUCTORS: [&str; 3] = ["builder(", "new(", "default("];

    // Paths that name the type through the crate: `reqwest::Client`, plus any crate alias.
    let mut qualified = vec![
        "reqwest::Client".to_owned(),
        "reqwest::ClientBuilder".to_owned(),
    ];
    // Local names bound to the type by an import, including renamed and glob imports.
    let mut imported = Vec::new();
    for statement in use_statements(text) {
        let Some(rest) = statement.strip_prefix("use reqwest") else {
            continue;
        };
        if let Some(alias) = rest.strip_prefix(" as ") {
            let alias = alias.trim();
            qualified.push(format!("{alias}::Client"));
            qualified.push(format!("{alias}::ClientBuilder"));
            continue;
        }
        let Some(items) = rest.strip_prefix("::") else {
            continue;
        };
        if items.contains('*') {
            // A glob import brings both names into scope under their own names.
            imported.push("Client".to_owned());
            imported.push("ClientBuilder".to_owned());
        }
        for item in items.trim_matches(['{', '}']).split(',') {
            let (path, alias) = match item.trim().split_once(" as ") {
                Some((path, alias)) => (path.trim(), Some(alias.trim())),
                None => (item.trim(), None),
            };
            if !matches!(path, "Client" | "ClientBuilder") {
                continue;
            }
            imported.push(alias.unwrap_or(path).to_owned());
        }
    }

    let forms = qualified
        .iter()
        .chain(imported.iter())
        .flat_map(|path| {
            CONSTRUCTORS
                .iter()
                .map(move |constructor| format!("{path}::{constructor}"))
        })
        .collect::<Vec<_>>();

    let mut found = Vec::new();
    for (offset, line) in text.lines().enumerate() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        if forms.iter().any(|form| code.contains(form.as_str())) {
            found.push((offset + 1, code.to_owned()));
        }
    }
    found
}

/// Every `use` statement in the file, whitespace-collapsed onto one line.
///
/// rustfmt breaks a long import across lines, so a line-at-a-time scan would miss
/// `use reqwest::{\n    Client,\n};` - which is exactly the form a file with several
/// reqwest imports ends up with.
fn use_statements(text: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        let statement = match current.as_mut() {
            Some(statement) => {
                statement.push(' ');
                statement.push_str(line);
                statement
            }
            None if line.starts_with("use ") => current.insert(line.to_owned()),
            None => continue,
        };
        if statement.ends_with(';') {
            let complete = statement
                .trim_end_matches(';')
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            statements.push(complete.replace("{ ", "{").replace(" }", "}"));
            current = None;
        }
    }
    statements
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
        for (line, code) in reqwest_constructions(&text) {
            offenders.push(format!("{}:{line}: {code}", source.display()));
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

/// The forms this guard claims to recognize, and the ones it must not flag.
///
/// The name is deliberately bounded: a textual guard cannot recognize *every* way to
/// reach a client (see `reqwest_constructions` for the stated residual), and a test named
/// "every form" would invite the next reader to trust it as complete.
#[test]
fn the_guard_recognizes_qualified_imported_renamed_and_glob_forms() {
    for source in [
        "let client = reqwest::Client::builder().build();",
        "let client = reqwest::Client::new();",
        "let client = reqwest::Client::default();",
        "let client = reqwest::ClientBuilder::new().build();",
        "let client = reqwest::ClientBuilder::default().build();",
        "use reqwest::Client;\nlet client = Client::default();",
        "use reqwest::{Client, Proxy};\nlet client = Client::builder().build();",
        "use reqwest::ClientBuilder;\nlet client = ClientBuilder::new().build();",
        "use reqwest::{Proxy, ClientBuilder};\nlet client = ClientBuilder::default().build();",
        // Renamed, aliased, glob, and multi-line imports all bind the same type.
        "use reqwest::Client as Http;\nlet client = Http::new();",
        "use reqwest::ClientBuilder as Builder;\nlet client = Builder::default().build();",
        "use reqwest as rq;\nlet client = rq::Client::new();",
        "use reqwest as rq;\nlet client = rq::ClientBuilder::new().build();",
        "use reqwest::*;\nlet client = Client::new();",
        "use reqwest::{\n    Client,\n    Proxy,\n};\nlet client = Client::builder().build();",
        "use reqwest::{\n    header::HeaderMap,\n    Client,\n};\nlet client = Client::new();",
    ] {
        assert_eq!(
            reqwest_constructions(source).len(),
            1,
            "unguarded reqwest construction: {source}"
        );
    }

    for source in [
        "// let client = reqwest::Client::new();",
        "use zuno_network::Client;\nlet client = Client::new();",
        "let client = zuno_network::client(purpose)?;",
        // A same-named type from another crate must not be flagged.
        "use hyper::Client;\nlet client = Client::new();",
        // Importing something else from reqwest binds no client name.
        "use reqwest::header::HeaderMap;\nlet client = Client::new();",
        "use reqwest::Proxy;\nlet client = Client::builder().build();",
    ] {
        assert!(
            reqwest_constructions(source).is_empty(),
            "false positive: {source}"
        );
    }
}

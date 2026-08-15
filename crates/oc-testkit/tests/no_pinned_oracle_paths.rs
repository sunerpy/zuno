//! The structural guard behind "the release is pinned, the route to it is not".
//!
//! Nine test files across six crates used to hard-code
//! `/config/.local/share/mise/installs/opencode/1.18.12/opencode`. Each one worked,
//! and that was the problem: they selected a release **other** than
//! [`oc_testkit::PINNED_RELEASE`] while every report in this workspace attributed
//! its measurements to the pin, and on any machine without that exact path they
//! degraded to a skip that read as a pass. The centralized oracle exists to close
//! that seam, and a rule that lives only in the oracle's docs is a rule the tenth
//! file will break.
//!
//! So the rule is executable here, in two directions:
//!
//! * **negative** — no test file may name a package-manager install path in code;
//! * **positive** — the differentials that drive an installed binary must still be
//!   routed through the oracle, so the seam cannot be "closed" by deleting coverage.
//!
//! # What is still allowed, and why
//!
//! A *comment* may name an old path: that is how the history of a defect stays
//! readable, and a comment cannot select a binary. A fixture may still carry a
//! release in its **file name** when its provenance is an executable assertion —
//! `.omo/fixtures/oracle-openapi-1.18.18.json` is retained on exactly those terms,
//! because `compat_suite.rs` refetches `/doc` from the running pinned release and
//! compares the bytes. A name that no test re-derives would be a claim, not a
//! fixture.

use std::path::{Path, PathBuf};

/// Substrings that betray a route chosen by a source file rather than discovered.
///
/// `installs/` is one package manager's layout; `opencode/1.` is any version-pinned
/// directory, whatever put it there.
const INSTALL_MARKERS: &[&str] = &["installs/opencode", "opencode/1."];

/// Differentials that execute an installed `opencode` and must resolve it centrally.
///
/// An inventory rather than a pattern, because the failure this prevents is a file
/// quietly dropping its oracle call — which no pattern over the *remaining* text can
/// see. Renaming or removing one of these fails here, loudly, with the reason.
const ROUTED_DIFFERENTIALS: &[&str] = &[
    "oc-catalog/tests/agent_differential.rs",
    "oc-catalog/tests/skill_differential.rs",
    "oc-cli/tests/differential.rs",
    "oc-cli/tests/rollback.rs",
    "oc-db/tests/message_export.rs",
    "oc-db/tests/schema.rs",
    "oc-db/tests/session.rs",
    "oc-llm/tests/catalog_differential.rs",
    "oc-lsp/tests/live_servers.rs",
    "oc-paths/tests/differential.rs",
    "oc-tools/tests/registry.rs",
    "oc-tools/tests/search_differential.rs",
];

/// The helper every one of those files must reach the installed release through.
const CENTRAL_RESOLVER: &str = "pinned_oracle";

fn crates_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/oc-testkit has a parent")
        .to_path_buf()
}

/// Every `.rs` file under any `crates/*/tests/` directory.
fn test_sources() -> Vec<PathBuf> {
    let root = crates_root();
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
        .map(walkdir::DirEntry::into_path)
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter(|path| {
            path.strip_prefix(&root).is_ok_and(|rel| {
                rel.components()
                    .nth(1)
                    .is_some_and(|c| c.as_os_str() == "tests")
            })
        })
        .collect();
    files.sort();
    assert!(
        files.len() > 20,
        "the walk found only {} test sources under {}, so it is not looking where it thinks",
        files.len(),
        root.display()
    );
    files
}

/// `true` for a line that cannot influence which binary runs.
fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//") || trimmed.starts_with("*") || trimmed.starts_with("#")
}

#[test]
fn no_test_file_selects_an_oracle_by_package_manager_path() {
    let this_file = Path::new(file!())
        .file_name()
        .expect("this test file has a name")
        .to_owned();
    let mut offenders = Vec::new();

    for path in test_sources() {
        if path.file_name() == Some(this_file.as_os_str()) {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for (number, line) in text.lines().enumerate() {
            if is_comment(line) {
                continue;
            }
            if let Some(marker) = INSTALL_MARKERS.iter().find(|m| line.contains(**m)) {
                offenders.push(format!(
                    "{}:{}: names {marker} in code: {}",
                    path.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these test files select an oracle by a path they wrote down:\n  {}\n\n\
         A written-down path can select a release other than {} — which is what made \
         several differentials measure 1.18.12 while every report named a different \
         build — and it exists on one machine, so elsewhere the test degrades to a \
         skip that reads as a pass. Resolve the binary through \
         oc_testkit::{CENTRAL_RESOLVER}_or_skip instead; it discovers the route and \
         refuses any candidate that does not report the pin. A comment may still \
         record an old path.",
        offenders.join("\n  "),
        oc_testkit::PINNED_RELEASE,
    );
}

#[test]
fn every_installed_binary_differential_still_routes_through_the_oracle() {
    let root = crates_root();
    let mut broken = Vec::new();

    for relative in ROUTED_DIFFERENTIALS {
        let path = root.join(relative);
        let Ok(text) = std::fs::read_to_string(&path) else {
            broken.push(format!("{relative}: missing"));
            continue;
        };
        if !text.contains(CENTRAL_RESOLVER) {
            broken.push(format!("{relative}: no reference to {CENTRAL_RESOLVER}"));
        }
    }

    assert!(
        broken.is_empty(),
        "these differentials no longer resolve the installed release centrally:\n  {}\n\n\
         Each one runs the real `opencode`, so it must obtain it from \
         oc_testkit::{CENTRAL_RESOLVER}_or_skip, which screens the candidate against \
         {}. If a differential was deliberately retired, remove it from \
         ROUTED_DIFFERENTIALS in the same change and say why — deleting the oracle \
         call while keeping the test is how coverage disappears silently.",
        broken.join("\n  "),
        oc_testkit::PINNED_RELEASE,
    );
}

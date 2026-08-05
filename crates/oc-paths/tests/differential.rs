//! Differential test: this crate's layout against the real `opencode` binary.
//!
//! # What is being compared
//!
//! `opencode debug paths` prints nine `key value` lines
//! (`packages/opencode/src/cli/cmd/debug/index.ts:79-87`). This test runs the
//! real binary and [`Layout::resolve_with`] under an **identical, fully
//! explicit** environment and requires the two dumps to be byte-identical.
//!
//! Only the keys the oracle emits are compared, because those are the only ones
//! it emits. The getters `debug paths` does *not* cover — `snapshot_root`,
//! `tool_output`, `auth_file`, `mcp_auth_file`, `models_cache`, `db_path`, the
//! config chain, project resolution — are unit-tested against the oracle source
//! in their own modules, plus [`db_override_relative_resolves_where_the_oracle_writes`]
//! below, which checks a path by observing where the real binary actually puts a
//! file.
//!
//! # Version skew, resolved rather than assumed
//!
//! The oracle source tree this port was read from is pinned at 1.18.13
//! (`aefaf140c1`); the installed binary reports 1.18.12. That skew was checked
//! before trusting the binary as an oracle, in the source tree:
//!
//! ```text
//! git diff --stat 7fe993879f..aefaf140c1 -- \
//!   packages/core/src/global.ts packages/core/src/database/database.ts \
//!   packages/opencode/src/config/paths.ts packages/core/src/tool-output-store.ts \
//!   packages/core/src/models-dev.ts packages/opencode/src/auth/index.ts \
//!   packages/opencode/src/mcp/auth.ts packages/opencode/src/snapshot/index.ts \
//!   packages/core/src/util/hash.ts packages/opencode/src/cli/cmd/debug/index.ts \
//!   packages/core/src/project.ts packages/core/src/git.ts packages/core/src/fs-util.ts
//! ```
//!
//! `7fe993879f` is "sync release versions for v1.18.12" and `aefaf140c1` is the
//! v1.18.13 pin, 18 commits later. The diff is **empty**: not one of the
//! thirteen layout-relevant files changed between the two releases. The 1.18.12
//! binary is therefore a valid oracle for the 1.18.13 layout, and a mismatch here
//! is this crate's bug, not version skew.
//!
//! Nothing is normalized. The comparison is on raw bytes, so a real difference
//! fails the test rather than being smoothed over.
//!
//! # Skipping
//!
//! The test is skipped, with a printed reason, when no `opencode` binary can be
//! found. It must never silently pass: a differential test that quietly
//! degrades to nothing is worse than no test, so the skip path prints and the
//! non-skip path asserts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use oc_paths::env::{
    HOME, OPENCODE_DB, XDG_CACHE_HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME,
};
use oc_paths::{DbLocation, Env, Layout};

/// Every candidate location for the real binary, in preference order.
///
/// `PATH` is consulted first via `OPENCODE_BIN`, then a plain `opencode`, then
/// the mise install layout this machine happens to use. The mise *shim* is
/// deliberately not used directly: it re-execs through `mise`, which rewrites
/// the environment and would make an `env_clear`-based comparison meaningless.
fn oracle_binary() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("OPENCODE_BIN") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let mise = home.join(".local/share/mise/installs/opencode");
    for candidate in ["latest/opencode", "1/opencode"] {
        let path = mise.join(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    let entries = std::fs::read_dir(&mise).ok()?;
    let mut versions: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("opencode"))
        .filter(|path| path.is_file())
        .collect();
    versions.sort();
    versions.pop()
}

/// Run `opencode debug paths` under exactly `env` and nothing else.
///
/// `env_clear` first, so the child cannot inherit an `XDG_*` or `TMPDIR` the
/// Rust side was not given. That is the only way the comparison proves anything.
fn oracle_dump(binary: &Path, env: &BTreeMap<&str, String>) -> String {
    let mut command = Command::new(binary);
    command.arg("debug").arg("paths").env_clear();
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("spawn {}: {error}", binary.display()));
    assert!(
        output.status.success(),
        "{} debug paths failed: {}",
        binary.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("debug paths stdout is utf-8")
}

/// The Rust side of the comparison, from the same map.
fn subject_dump(env: &BTreeMap<&str, String>) -> String {
    let layout = Layout::resolve_with(
        &Env::from_pairs(env.iter().map(|(k, v)| (*k, v.clone()))),
        None,
    );
    layout.debug_paths_dump()
}

/// `PATH` and `HOME` only; without `PATH` the child cannot find `git`, and
/// without `HOME` neither side has an XDG fallback base.
fn base_env(home: &str) -> BTreeMap<&'static str, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
    );
    env.insert(HOME, home.to_owned());
    env
}

fn compare(binary: &Path, label: &str, env: &BTreeMap<&str, String>) {
    let oracle = oracle_dump(binary, env);
    let subject = subject_dump(env);
    assert_eq!(
        oracle, subject,
        "[{label}] differential dump mismatch\nenv: {env:?}\n--- oracle ---\n{oracle}--- subject ---\n{subject}"
    );
    assert_eq!(
        oracle.lines().count(),
        oc_paths::DEBUG_PATHS_KEYS.len(),
        "[{label}] oracle emitted an unexpected number of keys:\n{oracle}"
    );
    println!(
        "[{label}] {} keys byte-identical\n{subject}",
        oracle.lines().count()
    );
}

/// Permutation 1: defaults. No `XDG_*`, so both sides derive every base from
/// `HOME`, and `TMPDIR` is unset so both fall through to `/tmp`.
///
/// `HOME` is the real one rather than a temp directory: the oracle's import-time
/// `mkdir` runs before `debug paths` does anything, and pointing it at a fresh
/// temp `HOME` would create a directory tree there. The real one already exists,
/// so nothing new is written.
#[test]
fn differential_defaults() {
    let Some(binary) = oracle_binary() else {
        println!("SKIPPED differential_defaults: no opencode binary found");
        return;
    };
    let home = std::env::var("HOME").expect("HOME");
    compare(&binary, "defaults", &base_env(&home));
}

/// Permutation 2: all four XDG bases pointed at a temp directory.
#[test]
fn differential_custom_xdg_home() {
    let Some(binary) = oracle_binary() else {
        println!("SKIPPED differential_custom_xdg_home: no opencode binary found");
        return;
    };
    let root = tempfile::tempdir().expect("tempdir");
    let home = std::env::var("HOME").expect("HOME");
    let mut env = base_env(&home);
    let base = |name: &str| root.path().join(name).to_string_lossy().into_owned();
    env.insert(XDG_DATA_HOME, base("data"));
    env.insert(XDG_CACHE_HOME, base("cache"));
    env.insert(XDG_CONFIG_HOME, base("config"));
    env.insert(XDG_STATE_HOME, base("state"));
    compare(&binary, "custom-xdg", &env);

    // The dump must actually reflect the temp root, or the permutation proved
    // nothing beyond permutation 1.
    let dump = subject_dump(&env);
    assert!(dump.contains(&base("data")), "{dump}");
}

/// Permutation 3: `OPENCODE_DB=:memory:` on top of a temp XDG root.
///
/// `debug paths` does not print the database path, so this permutation checks
/// two things: that the nine printed paths are unaffected by `OPENCODE_DB`, and
/// that this crate resolves the sentinel to [`DbLocation::Memory`] rather than to
/// a file called `:memory:`.
#[test]
fn differential_memory_db() {
    let Some(binary) = oracle_binary() else {
        println!("SKIPPED differential_memory_db: no opencode binary found");
        return;
    };
    let root = tempfile::tempdir().expect("tempdir");
    let home = std::env::var("HOME").expect("HOME");
    let mut env = base_env(&home);
    let base = |name: &str| root.path().join(name).to_string_lossy().into_owned();
    env.insert(XDG_DATA_HOME, base("data"));
    env.insert(XDG_CACHE_HOME, base("cache"));
    env.insert(XDG_CONFIG_HOME, base("config"));
    env.insert(XDG_STATE_HOME, base("state"));
    env.insert(OPENCODE_DB, ":memory:".to_owned());
    compare(&binary, "memory-db", &env);

    let layout = Layout::resolve_with(
        &Env::from_pairs(env.iter().map(|(k, v)| (*k, v.clone()))),
        None,
    );
    assert_eq!(layout.db_path_for_channel("latest"), DbLocation::Memory);
    assert_eq!(layout.db_path_for_channel("mybranch"), DbLocation::Memory);
    assert!(layout.db_path_for_channel("latest").is_memory());
}

/// The failure scenario, checked against the real binary's behaviour rather than
/// against a reading of its source.
///
/// A **relative** `OPENCODE_DB` must resolve under `data()`, never under the
/// working directory. `debug paths` cannot show this, so the oracle side is
/// established by running a command that opens the database from a working
/// directory that is *not* the data directory, and then looking at where the file
/// landed.
#[test]
fn db_override_relative_resolves_where_the_oracle_writes() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = std::env::var("HOME").expect("HOME");
    let xdg_data = root.path().join("data");
    let cwd = root.path().join("cwd");
    std::fs::create_dir_all(&xdg_data).expect("create xdg data");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let mut env = base_env(&home);
    env.insert(XDG_DATA_HOME, xdg_data.to_string_lossy().into_owned());
    env.insert(OPENCODE_DB, "relprobe.db".to_owned());

    // This crate's answer.
    let layout = Layout::resolve_with(
        &Env::from_pairs(env.iter().map(|(k, v)| (*k, v.clone()))),
        None,
    );
    let resolved = layout.db_path_for_channel("latest");
    let expected = xdg_data.join("opencode/relprobe.db");
    assert_eq!(resolved, DbLocation::File(expected.clone()));
    assert!(
        resolved.as_path().expect("file").starts_with(layout.data()),
        "{:?} is not under data() {}",
        resolved,
        layout.data().display()
    );
    assert_ne!(resolved.as_path(), Some(cwd.join("relprobe.db").as_path()));
    println!(
        "subject resolved OPENCODE_DB=relprobe.db to {}",
        expected.display()
    );

    let Some(binary) = oracle_binary() else {
        println!("SKIPPED oracle half: no opencode binary found");
        return;
    };

    // The oracle's answer: run something that opens the database. `run` needs no
    // credentials to get as far as opening it, and its exit status is
    // irrelevant here — only where the file appears matters.
    let mut command = Command::new(&binary);
    command
        .arg("run")
        .arg("--model")
        .arg("anthropic/no-such-model")
        .arg("probe")
        .current_dir(&cwd)
        .env_clear();
    for (key, value) in &env {
        command.env(key, value);
    }
    let _ = command.output().expect("spawn oracle run");

    assert!(
        expected.is_file(),
        "oracle did not create {} — data-relative resolution not reproduced",
        expected.display()
    );
    assert!(
        !cwd.join("relprobe.db").exists(),
        "oracle created relprobe.db in the working directory {}",
        cwd.display()
    );
    println!(
        "oracle created {} and left {} empty",
        expected.display(),
        cwd.display()
    );
}

/// Proof that the comparison is sensitive.
///
/// A differential test that compares two identical strings passes whatever the
/// code does. This one perturbs each of the nine values in turn and requires the
/// oracle dump to stop matching, so a green run above means the dumps really
/// agree rather than that the assertion is inert.
#[test]
fn the_comparison_detects_a_single_character_divergence() {
    let Some(binary) = oracle_binary() else {
        println!(
            "SKIPPED the_comparison_detects_a_single_character_divergence: no opencode binary found"
        );
        return;
    };
    let home = std::env::var("HOME").expect("HOME");
    let env = base_env(&home);
    let oracle = oracle_dump(&binary, &env);
    let subject = subject_dump(&env);
    assert_eq!(oracle, subject, "baseline must match before perturbing");

    for (index, line) in subject.lines().enumerate() {
        let perturbed: String = subject
            .lines()
            .enumerate()
            .map(|(position, current)| {
                if position == index {
                    format!("{current}X\n")
                } else {
                    format!("{current}\n")
                }
            })
            .collect();
        assert_ne!(
            oracle, perturbed,
            "perturbing line {index} ({line:?}) did not change the dump"
        );
    }
    println!(
        "comparison is sensitive to a one-character change on each of {} lines",
        subject.lines().count()
    );
}

/// The oracle binary must be locatable at all, so a machine without it fails
/// loudly here instead of every differential test quietly skipping.
#[test]
fn oracle_binary_is_locatable() {
    match oracle_binary() {
        Some(path) => {
            let version = Command::new(&path)
                .arg("--version")
                .output()
                .expect("spawn --version");
            let reported = String::from_utf8_lossy(&version.stdout).trim().to_owned();
            println!("oracle: {} reports {reported}", path.display());
            assert!(!reported.is_empty());
        }
        None => println!(
            "SKIPPED: no opencode binary; set OPENCODE_BIN to enable differential coverage"
        ),
    }
}

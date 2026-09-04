//! Post-edit formatter execution: detection, disablement, and failure handling.
//!
//! # Why every formatter here is a shell script this file writes
//!
//! The formatters in the built-in table are real programs, and this crate refuses
//! to download or install one (the same line todo 41 held for the ripgrep binary).
//! So each test writes its own executable into a `tempfile::tempdir()` and points a
//! stub [`ProgramLocator`] at it. That buys three things a real formatter would not:
//!
//! - **determinism** — the stub rewrites the file to a byte-exact known value, so
//!   "did it format" is an equality check rather than a guess about a real
//!   formatter's style;
//! - **a controllable failure** — a real formatter's non-zero exit depends on its
//!   version and on the input's syntax; a stub's is a literal `exit 3` with a
//!   literal stderr, so the stderr assertion can be exact;
//! - **hostility on demand** — the truncating stub damages the file *and then*
//!   fails, which is the case that decides whether an edit can be lost.
//!
//! # The shared-namespace rule
//!
//! Every fixture is inside a per-test `tempdir()`, so no test names a fixed path,
//! and nothing here binds a port, reads `PATH`, or mutates the environment. The
//! program locator is injected precisely so no test has to touch the environment to
//! be believed: a test may not assume exclusive use of a shared namespace.
//!
//! # Why fixtures never edit a file to the same length
//!
//! `zuno-snapshot`'s flake was a same-size edit inside one second of a commit being
//! invisible to git's stat cache. Nothing here consults git, but the habit is cheap
//! and the assertions are stronger for it: every stub changes the file's **length**
//! as well as its content, so "the bytes changed" cannot be true by coincidence.

#[cfg(unix)]
use async_trait::async_trait;
use serde_json::json;
#[cfg(unix)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;
use tempfile::TempDir;
use zuno_config::schema::formatter::{FormatterConfig, FormatterEntry};
#[cfg(unix)]
use zuno_error::ToolError;
#[cfg(unix)]
use zuno_tool::{NeverInterrupted, PermissionAsk, PermissionAsker, ToolContext};
#[cfg(unix)]
use zuno_tools::FileTools;
use zuno_tools::FormatOutcome;
use zuno_tools::format::{
    Availability, DEFINITIONS, FailureKind, Formatters, ProgramLocator, builtin,
};

/// The bytes every fixture starts from, and the bytes the stub rewrites them to.
///
/// Different lengths on purpose; see the module docs.
const BEFORE: &str = "alpha\n";
#[cfg(unix)]
const AFTER: &str = "formatted by the stub\n";

#[cfg(unix)]
#[derive(Default)]
struct AllowAll;

#[cfg(unix)]
#[async_trait]
impl PermissionAsker for AllowAll {
    async fn ask(
        &self,
        _origin: zuno_tool::PermissionOrigin<'_>,
        _tool: &str,
        _ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        Ok(())
    }
}

#[cfg(unix)]
fn context() -> ToolContext {
    ToolContext::new(
        "session-format",
        "message-format",
        "call-format",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

/// A locator that answers from a fixed table, so no test reads `PATH`.
#[cfg(unix)]
#[derive(Debug, Default)]
struct StubPrograms(BTreeMap<String, PathBuf>);

#[cfg(unix)]
impl StubPrograms {
    fn with(mut self, program: &str, path: &Path) -> Self {
        self.0.insert(program.to_owned(), path.to_path_buf());
        self
    }
}

#[cfg(unix)]
impl ProgramLocator for StubPrograms {
    fn locate(&self, program: &str) -> Option<PathBuf> {
        self.0.get(program).cloned()
    }
}

/// A locator that finds nothing, which is what an unconfigured machine looks like.
#[derive(Debug, Default)]
struct NoPrograms;

impl ProgramLocator for NoPrograms {
    fn locate(&self, _program: &str) -> Option<PathBuf> {
        None
    }
}

/// Resolve the last positional argument, which is where `$FILE` lands in every
/// built-in command: `clang-format -i $FILE` puts the flag in `$1`, not the file.
#[cfg(unix)]
const LAST_ARGUMENT: &str = "for target in \"$@\"; do :; done\n";

/// The argument [`wait_until_executable`] probes a freshly written stub with.
#[cfg(unix)]
const PROBE: &str = "--probe-executable";

/// How long a stub's `ETXTBSY` window is allowed to last.
///
/// The window is bounded by how long a concurrently forked child takes to reach
/// `execve`, which is microseconds. 8 × 5 ms is three orders of magnitude of
/// headroom; a measured 12-thread run needed at most a single retry per stub.
#[cfg(unix)]
const PROBE_ATTEMPTS: usize = 8;
#[cfg(unix)]
const PROBE_BACKOFF: Duration = Duration::from_millis(5);

/// Write an executable shell script, returning its path.
///
/// `body` omits the shebang: this adds it, plus an early exit for [`PROBE`], so
/// [`wait_until_executable`] can prove the file is executable without running it
/// for effect.
#[cfg(unix)]
fn script(directory: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = directory.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\n[ \"$1\" = '{PROBE}' ] && exit 0\n{body}"),
    )
    .expect("write stub");
    let mut permissions = std::fs::metadata(&path).expect("stat stub").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod stub");
    wait_until_executable(&path);
    path
}

/// Block until `path` can actually be `execve`d, defeating `ETXTBSY`.
///
/// **This is not defensive padding; without it this file flakes under load.**
/// `cargo test` runs a target's tests as threads in one process. `execve` fails
/// with `ETXTBSY` while *any* process holds the target write-open, and a sibling
/// test's `fork` — every one of these tests spawns processes — snapshots the fd
/// table while this thread's write fd to the stub is still open, so the forked
/// child holds a copy of it until it reaches its own `execve`.
///
/// Measured on this machine with a standalone harness: 6 threads writing and
/// exec'ing 2,000 stubs each alongside 6 threads forking `/bin/sh` produced
/// **1,342 `ETXTBSY` failures in 12,000 attempts (11%)**, and the same workload
/// with one writer and no concurrent forkers produced **0 in 2,000**. That is the
/// signature of this project's load-correlated flake family.
///
/// A bounded retry is a real fix rather than a mask because the condition is
/// **self-limiting**: nothing ever write-opens the inode again after this
/// function returns, so once the borrowed fd is gone it cannot come back. The
/// same standalone harness recovered 1,063 of 1,063 transient failures with this
/// bound and left 0 unrecovered.
///
/// Production is deliberately untouched: opencode does not write the formatter it
/// then executes, so it cannot lend out an fd to one. A formatter installed by a
/// package manager mid-session could in principle hit this, and it is already
/// reported cleanly as `NotSpawned` with the OS message.
#[cfg(unix)]
fn wait_until_executable(path: &Path) {
    for attempt in 0..PROBE_ATTEMPTS {
        let probe = std::process::Command::new(path)
            .arg(PROBE)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match probe {
            Ok(_) => return,
            // 26 is ETXTBSY. `ErrorKind` has no variant for it on stable.
            Err(error) if error.raw_os_error() == Some(26) && attempt + 1 < PROBE_ATTEMPTS => {
                std::thread::sleep(PROBE_BACKOFF);
            }
            Err(error) => panic!("stub {path:?} never became executable: {error}"),
        }
    }
}

/// The body that rewrites the file to [`AFTER`].
#[cfg(unix)]
fn rewrite_body() -> String {
    format!("{LAST_ARGUMENT}printf '%s' '{AFTER}' > \"$target\"\n")
}

/// A stub that rewrites the file to [`AFTER`] and succeeds.
#[cfg(unix)]
fn rewriting_stub(directory: &Path) -> PathBuf {
    script(directory, "stub-format", &rewrite_body())
}

/// A stub that leaves the file alone and exits non-zero with a known stderr.
#[cfg(unix)]
fn failing_stub(directory: &Path) -> PathBuf {
    script(
        directory,
        "stub-fail",
        "echo 'stub-fail: cannot parse line 1: unexpected token' >&2\nexit 3\n",
    )
}

/// A stub that truncates the file *and then* fails — the case that decides whether
/// a formatter can cost an edit.
#[cfg(unix)]
fn destructive_stub(directory: &Path) -> PathBuf {
    script(
        directory,
        "stub-destroy",
        &format!(
            "{LAST_ARGUMENT}: > \"$target\"\necho 'stub-destroy: bailed out after truncating' >&2\nexit 1\n"
        ),
    )
}

/// A stub that records every path it was handed, one per line.
#[cfg(unix)]
fn recording_stub(directory: &Path, log: &Path) -> PathBuf {
    script(
        directory,
        "stub-record",
        &format!(
            "{LAST_ARGUMENT}echo \"$target\" >> '{log}'\nprintf '%s' '{AFTER}' > \"$target\"\n",
            log = log.display()
        ),
    )
}

/// A stub that never returns, so a ceiling can be observed.
#[cfg(unix)]
fn hanging_stub(directory: &Path) -> PathBuf {
    script(directory, "stub-hang", "sleep 600\n")
}

/// An argv naming `program` by absolute path.
///
/// A **configured** command is taken verbatim — `format/index.ts:154` replaces the
/// availability probe with `async () => info.command ?? false` — so it is spawned
/// exactly as written and never resolved through the locator. That is why these
/// fixtures give an absolute path: relying on `PATH` would mean mutating the
/// environment, which is `unsafe` and forbidden here. The locator still governs the
/// **built-ins**, which is what the availability tests exercise.
fn command(program: &Path, rest: &[&str]) -> Vec<String> {
    let mut argv = vec![program.to_string_lossy().into_owned()];
    argv.extend(rest.iter().map(|&item| item.to_owned()));
    argv
}

/// One formatter override, spelled the way a user's config spells it.
fn entry(
    command: Option<Vec<String>>,
    extensions: Option<&[&str]>,
    disabled: Option<bool>,
    environment: Option<&[(&str, &str)]>,
) -> FormatterEntry {
    FormatterEntry {
        disabled,
        command,
        environment: environment.map(|pairs| {
            pairs
                .iter()
                .map(|&(key, value)| (key.to_owned(), value.to_owned()))
                .collect()
        }),
        extensions: extensions.map(|list| list.iter().map(|&item| item.to_owned()).collect()),
    }
}

/// Parse a `formatter` config from JSON, so the tests exercise the real union
/// rather than a hand-built enum a typo could not reach.
fn config(raw: &str) -> FormatterConfig {
    serde_json::from_str(raw).expect("formatter config parses")
}

fn resolved(raw: &str) -> zuno_catalog::formatter::ResolvedFormatters {
    zuno_catalog::formatter::ResolvedFormatters::resolve(Some(&config(raw)))
}

fn formatters(root: &Path, raw: &str, locator: Arc<dyn ProgramLocator>) -> Formatters {
    Formatters::new(root, root, &resolved(raw)).with_locator(locator)
}

/// A workspace with `subject.txt` holding [`BEFORE`], plus a `bin` directory.
fn workspace() -> (TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("temporary workspace");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).expect("bin directory");
    let subject = root.path().join("subject.txt");
    std::fs::write(&subject, BEFORE).expect("fixture");
    (root, bin, subject)
}

// ---------------------------------------------------------------------------
// The built-in table
// ---------------------------------------------------------------------------

#[test]
fn every_builtin_definition_has_a_command_and_claims_at_least_one_extension() {
    assert!(
        DEFINITIONS.len() >= 26,
        "the oracle exports 26 formatters; the table has {}",
        DEFINITIONS.len()
    );
    for definition in DEFINITIONS {
        assert!(
            !definition.name.is_empty(),
            "a definition with no name cannot be addressed by config"
        );
        assert!(
            !definition.command.is_empty(),
            "{} has no command",
            definition.name
        );
        assert!(
            !definition.program().is_empty(),
            "{}'s command has an empty program",
            definition.name
        );
        assert!(
            !definition.extensions.is_empty(),
            "{} claims no extension, so it could never run",
            definition.name
        );
        for extension in definition.extensions {
            assert!(
                extension.starts_with('.'),
                "{} claims {extension:?} without the leading dot path.extname() produces",
                definition.name
            );
        }
        assert!(
            definition.command.contains(&"$FILE"),
            "{} never names the file it is meant to format",
            definition.name
        );
    }
}

#[test]
fn the_builtin_names_are_unique_and_include_the_two_renamed_exports() {
    let mut names: Vec<&str> = DEFINITIONS.iter().map(|entry| entry.name).collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), total, "duplicate formatter name in the table");

    // `export const clang` has `name: "clang-format"` and `export const rlang` has
    // `name: "air"`; config keys on the `name`, not the export.
    assert!(builtin::definition("clang-format").is_some());
    assert!(builtin::definition("air").is_some());
    assert!(builtin::definition("clang").is_none());
    assert!(builtin::definition("rlang").is_none());
}

#[test]
fn the_node_hosted_formatters_carry_bun_be_bun() {
    for name in ["prettier", "oxfmt", "biome"] {
        let definition = builtin::definition(name).expect("built-in present");
        assert_eq!(
            definition.environment,
            builtin::BUN_BE_BUN,
            "{name} must set BUN_BE_BUN=1"
        );
    }
}

#[test]
fn the_table_carries_the_oracles_conditional_shapes() {
    let ruff = builtin::definition("ruff").expect("ruff");
    assert_eq!(ruff.availability, Availability::RuffConfig);

    let uv = builtin::definition("uv").expect("uv");
    assert_eq!(uv.shadowed_by, Some("ruff"), "uv stands down for ruff");

    let oxfmt = builtin::definition("oxfmt").expect("oxfmt");
    assert!(oxfmt.experimental, "oxfmt is behind a runtime flag");

    let clang = builtin::definition("clang-format").expect("clang-format");
    assert_eq!(
        clang.availability,
        Availability::ProgramWithMarker(&[".clang-format"])
    );

    let air = builtin::definition("air").expect("air");
    assert_eq!(
        air.availability,
        Availability::ProgramWithHelpFirstLine(&["R language", "formatter"])
    );

    let pint = builtin::definition("pint").expect("pint");
    assert_eq!(pint.command, &["./vendor/bin/pint", "$FILE"]);
}

#[test]
fn rust_and_python_are_claimed_by_the_formatters_that_should_claim_them() {
    let rust: Vec<&str> = builtin::for_extension(".rs")
        .map(|entry| entry.name)
        .collect();
    assert_eq!(rust, vec!["rustfmt"]);

    let python: Vec<&str> = builtin::for_extension(".py")
        .map(|entry| entry.name)
        .collect();
    assert_eq!(python, vec!["ruff", "uv"]);

    assert!(
        builtin::for_extension(".rs").all(|entry| !entry.claims(".py")),
        "the Rust formatter must not claim Python"
    );
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn a_configured_formatter_runs_after_an_edit_and_the_content_is_formatted() {
    let (root, bin, subject) = workspace();
    let stub = rewriting_stub(&bin);
    let runtime = Arc::new(formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-format", &stub)),
    ));

    let tools =
        FileTools::with_formatter(root.path(), runtime.clone()).expect("file tools with formatter");
    tools
        .read
        .execute(json!({ "filePath": subject }), context())
        .await
        .expect("read before edit");
    let output = tools
        .edit
        .execute(
            json!({ "filePath": subject, "edits": [{ "oldString": "alpha", "newString": "beta" }] }),
            context(),
        )
        .await
        .expect("edit");

    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        AFTER,
        "the formatter's output must be what is on disk after the edit"
    );
    assert_eq!(output.metadata.get("formatted"), Some(&json!(true)));
    assert!(
        output.metadata.get("formatterFailures").is_none(),
        "a successful format reports no failures"
    );
    assert!(
        !output.output.contains("Formatter"),
        "a successful format says nothing extra to the model: {:?}",
        output.output
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_configured_formatter_runs_after_a_write_and_after_a_patch() {
    let (root, bin, _subject) = workspace();
    let log = root.path().join("formatted.log");
    let stub = recording_stub(&bin, &log);
    let runtime = Arc::new(formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-record", &stub)),
    ));
    let tools = FileTools::with_formatter(root.path(), runtime).expect("file tools");

    tools
        .write
        .execute(
            json!({ "filePath": root.path().join("written.txt"), "content": BEFORE }),
            context(),
        )
        .await
        .expect("write");
    assert_eq!(
        std::fs::read_to_string(root.path().join("written.txt")).expect("written"),
        AFTER
    );

    let output = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Add File: patched.txt\n",
                    "+one\n",
                    "*** End Patch"
                )
            }),
            context(),
        )
        .await
        .expect("apply patch");
    assert_eq!(
        std::fs::read_to_string(root.path().join("patched.txt")).expect("patched"),
        AFTER
    );
    assert_eq!(
        output.metadata.get("formattedFiles"),
        Some(&json!([format!(
            "{}",
            root.path().join("patched.txt").display()
        )]))
    );

    let logged = std::fs::read_to_string(&log).expect("log");
    assert_eq!(
        logged.lines().count(),
        2,
        "the formatter should have been handed exactly the two written files: {logged:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_configured_command_replaces_the_builtins_and_environment_is_passed_through() {
    let (root, bin, subject) = workspace();
    let stub = script(
        &bin,
        "stub-env",
        &format!("{LAST_ARGUMENT}printf 'env=%s\\n' \"$STUB_MARKER\" > \"$target\"\n"),
    );
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "rustfmt": entry(
                Some(command(&stub, &["$FILE"])),
                Some(&[".txt"]),
                None,
                Some(&[("STUB_MARKER", "carried")]),
            ),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-env", &stub)),
    );

    let outcome = runtime.format_all(&subject).await;
    assert!(outcome.changed);
    assert!(!outcome.has_failures());
    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        "env=carried\n",
        "the configured environment must reach the formatter"
    );
    assert!(
        !runtime.claiming(".rs").any(|name| name == "rustfmt"),
        "a configured `extensions` replaces the built-in's list outright"
    );
    assert!(runtime.claiming(".txt").any(|name| name == "rustfmt"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_formatter_that_leaves_the_bytes_alone_reports_no_change() {
    let (root, bin, subject) = workspace();
    let stub = script(&bin, "stub-noop", "exit 0\n");
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-noop", &stub)),
    );

    let outcome = runtime.format_all(&subject).await;
    assert_eq!(outcome, FormatOutcome::default());
    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        BEFORE
    );
}

// ---------------------------------------------------------------------------
// Extension matching
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn a_formatter_does_not_run_on_a_file_it_does_not_claim() {
    let (root, bin, _subject) = workspace();
    let log = root.path().join("formatted.log");
    let stub = recording_stub(&bin, &log);
    // A Rust formatter, configured exactly as a user would configure one.
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "rustfmt": entry(Some(command(&stub, &["$FILE"])), Some(&[".rs"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-record", &stub)),
    );

    let python = root.path().join("module.py");
    std::fs::write(&python, BEFORE).expect("python fixture");
    let outcome = runtime.format_all(&python).await;

    assert_eq!(
        outcome,
        FormatOutcome::default(),
        "a Rust formatter must not touch a .py file"
    );
    assert_eq!(
        std::fs::read_to_string(&python).expect("read back"),
        BEFORE,
        "the .py file must be byte-identical"
    );
    assert!(
        !log.exists(),
        "the formatter must not have been spawned at all"
    );

    // The same formatter, on the extension it does claim, proves the stub works —
    // so the assertion above is about matching, not about a broken fixture.
    let rust = root.path().join("lib.rs");
    std::fs::write(&rust, BEFORE).expect("rust fixture");
    assert!(runtime.format_all(&rust).await.changed);
    assert_eq!(std::fs::read_to_string(&rust).expect("read back"), AFTER);
}

#[cfg(unix)]
#[tokio::test]
async fn a_file_with_no_extension_is_never_formatted() {
    let (root, bin, _subject) = workspace();
    let log = root.path().join("formatted.log");
    let stub = recording_stub(&bin, &log);
    let runtime = formatters(
        root.path(),
        // `extensions: [""]` is the config that would claim everything if the empty
        // extension were matched rather than short-circuited.
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[""]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-record", &stub)),
    );

    let makefile = root.path().join("Makefile");
    std::fs::write(&makefile, BEFORE).expect("fixture");
    assert_eq!(
        runtime.format_all(&makefile).await,
        FormatOutcome::default()
    );
    assert_eq!(
        std::fs::read_to_string(&makefile).expect("read back"),
        BEFORE
    );
    assert!(!log.exists());
}

#[test]
fn the_extension_is_the_final_segment_with_a_leading_dot() {
    assert_eq!(Formatters::extension_of(Path::new("a/b/c.rs")), ".rs");
    assert_eq!(
        Formatters::extension_of(Path::new("index.html.erb")),
        ".erb"
    );
    assert_eq!(Formatters::extension_of(Path::new("Makefile")), "");
    assert_eq!(Formatters::extension_of(Path::new(".gitignore")), "");
}

// ---------------------------------------------------------------------------
// disabled
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn the_global_switch_off_skips_every_formatter() {
    let (root, bin, subject) = workspace();
    let log = root.path().join("formatted.log");
    let stub = recording_stub(&bin, &log);
    let runtime = Formatters::new(
        root.path(),
        root.path(),
        &zuno_catalog::formatter::ResolvedFormatters::resolve(Some(&config("false"))),
    )
    .with_locator(Arc::new(StubPrograms::default().with("stub-record", &stub)));

    assert_eq!(runtime.names().count(), 0);
    assert_eq!(runtime.format_all(&subject).await, FormatOutcome::default());
    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        BEFORE
    );
    assert!(!log.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_per_formatter_disabled_true_skips_that_formatter_and_leaves_the_others() {
    let (root, bin, subject) = workspace();
    let log = root.path().join("formatted.log");
    let stub = recording_stub(&bin, &log);
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "off": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), Some(true), None),
            "on": entry(Some(command(&stub, &["$FILE"])), Some(&[".md"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-record", &stub)),
    );

    assert!(
        !runtime.contains("off"),
        "a disabled formatter is not in the registry"
    );
    assert!(runtime.contains("on"));

    assert_eq!(
        runtime.format_all(&subject).await,
        FormatOutcome::default(),
        "the .txt file's only formatter was disabled"
    );
    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        BEFORE
    );
    assert!(!log.exists());

    // The sibling that stayed enabled still runs, so the skip above is about
    // `disabled`, not about the fixture being inert.
    let markdown = root.path().join("notes.md");
    std::fs::write(&markdown, BEFORE).expect("markdown fixture");
    assert!(runtime.format_all(&markdown).await.changed);
    assert_eq!(
        std::fs::read_to_string(&markdown).expect("read back"),
        AFTER
    );
}

#[test]
fn disabling_a_builtin_removes_it_while_true_keeps_the_whole_table() {
    let root = tempfile::tempdir().expect("temporary workspace");

    let all = Formatters::new(
        root.path(),
        root.path(),
        &zuno_catalog::formatter::ResolvedFormatters::resolve(Some(&config("true"))),
    );
    assert_eq!(
        all.names().count(),
        DEFINITIONS.len(),
        "`formatter: true` enables the built-ins with no overrides"
    );

    let without = formatters(
        root.path(),
        &serde_json::to_string(&json!({ "rustfmt": entry(None, None, Some(true), None) }))
            .expect("config json"),
        Arc::new(NoPrograms),
    );
    assert!(!without.contains("rustfmt"));
    assert_eq!(without.claiming(".rs").count(), 0);
    assert_eq!(without.names().count(), DEFINITIONS.len() - 1);
}

#[test]
fn disabling_either_of_ruff_and_uv_disables_both() {
    let root = tempfile::tempdir().expect("temporary workspace");
    for name in ["ruff", "uv"] {
        let runtime = formatters(
            root.path(),
            &serde_json::to_string(&json!({ name: entry(None, None, Some(true), None) }))
                .expect("config json"),
            Arc::new(NoPrograms),
        );
        assert!(!runtime.contains("ruff"), "disabling {name} must drop ruff");
        assert!(!runtime.contains("uv"), "disabling {name} must drop uv");
        assert_eq!(runtime.claiming(".py").count(), 0);
    }
}

#[tokio::test]
async fn an_absent_formatter_key_disables_formatting_entirely() {
    let (root, _bin, subject) = workspace();
    let runtime = Formatters::new(
        root.path(),
        root.path(),
        &zuno_catalog::formatter::ResolvedFormatters::resolve(None),
    );
    assert_eq!(runtime.names().count(), 0);
    assert_eq!(runtime.format_all(&subject).await, FormatOutcome::default());
}

// ---------------------------------------------------------------------------
// Failure: the edit survives, and the stderr is surfaced
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn a_failing_formatter_is_reported_while_the_edit_persists() {
    let (root, bin, subject) = workspace();
    let stub = failing_stub(&bin);
    let runtime = Arc::new(formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-fail", &stub)),
    ));
    let tools = FileTools::with_formatter(root.path(), runtime).expect("file tools");

    tools
        .read
        .execute(json!({ "filePath": subject }), context())
        .await
        .expect("read before edit");
    let output = tools
        .edit
        .execute(
            json!({ "filePath": subject, "edits": [{ "oldString": "alpha", "newString": "beta" }] }),
            context(),
        )
        .await
        .expect("the edit must succeed even though the formatter failed");

    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        "beta\n",
        "the edit's bytes must be exactly what is on disk"
    );
    assert!(
        output.output.contains("Edit applied successfully."),
        "the edit is still reported as applied: {:?}",
        output.output
    );
    assert!(
        output
            .output
            .contains("stub-fail: cannot parse line 1: unexpected token"),
        "the formatter's stderr must reach the model: {:?}",
        output.output
    );
    assert!(
        output.output.contains("exited with status 3"),
        "the exit status must be named: {:?}",
        output.output
    );
    assert!(
        output.output.contains("The edit was written and is intact"),
        "the report must say the edit survived: {:?}",
        output.output
    );

    let failures = output
        .metadata
        .get("formatterFailures")
        .and_then(|value| value.as_array())
        .expect("failures in metadata");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["formatter"], json!("stub"));
    assert_eq!(failures[0]["exitCode"], json!(3));
    assert_eq!(failures[0]["editRestored"], json!(false));
    assert_eq!(
        failures[0]["stderr"],
        json!("stub-fail: cannot parse line 1: unexpected token\n")
    );
    assert_eq!(output.metadata.get("formatted"), Some(&json!(false)));
}

#[cfg(unix)]
#[tokio::test]
async fn a_formatter_that_truncates_the_file_before_failing_has_its_damage_undone() {
    let (root, bin, subject) = workspace();
    let stub = destructive_stub(&bin);
    let runtime = Arc::new(formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-destroy", &stub)),
    ));
    let tools = FileTools::with_formatter(root.path(), runtime).expect("file tools");

    tools
        .read
        .execute(json!({ "filePath": subject }), context())
        .await
        .expect("read before edit");
    let output = tools
        .edit
        .execute(
            json!({ "filePath": subject, "edits": [{ "oldString": "alpha", "newString": "beta" }] }),
            context(),
        )
        .await
        .expect("the edit must survive a destructive formatter");

    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        "beta\n",
        "the truncation must have been undone"
    );
    let failures = output
        .metadata
        .get("formatterFailures")
        .and_then(|value| value.as_array())
        .expect("failures in metadata");
    assert_eq!(failures[0]["editRestored"], json!(true));
    assert!(
        output
            .output
            .contains("the formatter's damage to the file was undone"),
        "a restore must be stated, not silent: {:?}",
        output.output
    );
}

#[tokio::test]
async fn a_formatter_that_cannot_be_spawned_is_reported_and_the_write_stands() {
    let (root, _bin, subject) = workspace();
    let missing = root.path().join("bin").join("not-installed");
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "ghost": entry(Some(command(&missing, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(NoPrograms),
    );

    let outcome = runtime.format_all(&subject).await;
    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(outcome.failures[0].kind, FailureKind::NotSpawned);
    assert!(!outcome.changed);
    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        BEFORE,
        "a formatter that never ran cannot have changed the file"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_hanging_formatter_is_abandoned_at_the_ceiling_and_the_write_stands() {
    let (root, bin, subject) = workspace();
    let stub = hanging_stub(&bin);
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-hang", &stub)),
    )
    .with_ceiling(Duration::from_millis(150));

    let outcome = runtime.format_all(&subject).await;
    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(
        outcome.failures[0].kind,
        FailureKind::TimedOut { after_seconds: 0 }
    );
    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        BEFORE
    );
}

#[cfg(unix)]
#[tokio::test]
async fn one_failing_formatter_does_not_stop_the_next_one() {
    let (root, bin, subject) = workspace();
    let failing = failing_stub(&bin);
    let rewriting = rewriting_stub(&bin);
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "first": entry(Some(command(&failing, &["$FILE"])), Some(&[".txt"]), None, None),
            "second": entry(Some(command(&rewriting, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(
            StubPrograms::default()
                .with("stub-fail", &failing)
                .with("stub-format", &rewriting),
        ),
    );

    let outcome = runtime.format_all(&subject).await;
    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(outcome.failures[0].formatter, "first");
    assert!(outcome.changed, "the second formatter still ran");
    assert_eq!(std::fs::read_to_string(&subject).expect("read back"), AFTER);
}

#[cfg(unix)]
#[tokio::test]
async fn a_patch_that_hits_a_failing_formatter_still_applies_every_operation() {
    let (root, bin, _subject) = workspace();
    let stub = failing_stub(&bin);
    let runtime = Arc::new(formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-fail", &stub)),
    ));
    let tools = FileTools::with_formatter(root.path(), runtime).expect("file tools");

    let output = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Add File: one.txt\n",
                    "+one\n",
                    "*** Add File: two.txt\n",
                    "+two\n",
                    "*** End Patch"
                )
            }),
            context(),
        )
        .await
        .expect("the patch must apply even though the formatter failed");

    assert_eq!(
        std::fs::read_to_string(root.path().join("one.txt")).expect("one"),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("two.txt")).expect("two"),
        "two\n"
    );
    let failures = output
        .metadata
        .get("formatterFailures")
        .and_then(|value| value.as_array())
        .expect("failures in metadata");
    assert_eq!(
        failures.len(),
        2,
        "both written files were offered to the formatter"
    );
}

// ---------------------------------------------------------------------------
// Availability
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_builtin_whose_program_is_absent_is_skipped() {
    let (root, _bin, _subject) = workspace();
    let runtime = Formatters::new(
        root.path(),
        root.path(),
        &zuno_catalog::formatter::ResolvedFormatters::resolve(Some(&config("true"))),
    )
    .with_locator(Arc::new(NoPrograms));

    let rust = root.path().join("lib.rs");
    std::fs::write(&rust, BEFORE).expect("rust fixture");
    assert_eq!(
        runtime.format_all(&rust).await,
        FormatOutcome::default(),
        "rustfmt is in the registry but not on this machine"
    );
    assert_eq!(std::fs::read_to_string(&rust).expect("read back"), BEFORE);
}

#[cfg(unix)]
#[tokio::test]
async fn a_marker_gated_builtin_runs_only_once_its_marker_exists() {
    let (root, bin, _subject) = workspace();
    let stub = rewriting_stub(&bin);
    let runtime = Formatters::new(
        root.path(),
        root.path(),
        &zuno_catalog::formatter::ResolvedFormatters::resolve(Some(&config("true"))),
    )
    .with_locator(Arc::new(
        StubPrograms::default().with("clang-format", &stub),
    ));

    let source = root.path().join("main.c");
    std::fs::write(&source, BEFORE).expect("c fixture");
    assert_eq!(
        runtime.format_all(&source).await,
        FormatOutcome::default(),
        "clang-format needs a .clang-format file"
    );

    std::fs::write(root.path().join(".clang-format"), "BasedOnStyle: LLVM\n")
        .expect("marker fixture");
    assert!(runtime.format_all(&source).await.changed);
    assert_eq!(std::fs::read_to_string(&source).expect("read back"), AFTER);
}

#[cfg(unix)]
#[tokio::test]
async fn the_experimental_formatter_stays_off_until_its_flag_is_set() {
    let (root, _bin, _subject) = workspace();
    std::fs::write(
        root.path().join("package.json"),
        json!({ "devDependencies": { "oxfmt": "1.0.0" } }).to_string(),
    )
    .expect("manifest fixture");
    let node_bin = root.path().join("node_modules").join(".bin");
    std::fs::create_dir_all(&node_bin).expect("node bin");
    script(&node_bin, "oxfmt", &rewrite_body());

    let source = root.path().join("app.ts");
    let base = Formatters::new(
        root.path(),
        root.path(),
        // Only oxfmt survives, so nothing else can claim the file and confuse the
        // result.
        &resolved(
            &serde_json::to_string(&json!({
                "prettier": entry(None, None, Some(true), None),
                "biome": entry(None, None, Some(true), None),
            }))
            .expect("config json"),
        ),
    )
    .with_locator(Arc::new(NoPrograms));

    std::fs::write(&source, BEFORE).expect("ts fixture");
    assert_eq!(
        base.clone().format_all(&source).await,
        FormatOutcome::default(),
        "oxfmt is behind experimentalOxfmt"
    );

    std::fs::write(&source, BEFORE).expect("ts fixture");
    assert!(
        base.with_experimental_oxfmt(true)
            .format_all(&source)
            .await
            .changed,
        "with the flag set, oxfmt formats"
    );
    assert_eq!(std::fs::read_to_string(&source).expect("read back"), AFTER);
}

#[cfg(unix)]
#[tokio::test]
async fn a_node_hosted_formatter_needs_both_the_declaration_and_the_binary() {
    let (root, _bin, _subject) = workspace();
    let source = root.path().join("app.ts");
    std::fs::write(&source, BEFORE).expect("ts fixture");
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "biome": entry(None, None, Some(true), None),
            "oxfmt": entry(None, None, Some(true), None),
        }))
        .expect("config json"),
        Arc::new(NoPrograms),
    );

    assert_eq!(
        runtime.format_all(&source).await,
        FormatOutcome::default(),
        "no package.json declares prettier"
    );

    std::fs::write(
        root.path().join("package.json"),
        json!({ "dependencies": { "prettier": "3.0.0" } }).to_string(),
    )
    .expect("manifest fixture");
    assert_eq!(
        runtime.format_all(&source).await,
        FormatOutcome::default(),
        "declared but not installed: node_modules/.bin/prettier is absent"
    );

    let node_bin = root.path().join("node_modules").join(".bin");
    std::fs::create_dir_all(&node_bin).expect("node bin");
    script(&node_bin, "prettier", &rewrite_body());
    assert!(runtime.format_all(&source).await.changed);
    assert_eq!(std::fs::read_to_string(&source).expect("read back"), AFTER);
}

#[cfg(unix)]
#[tokio::test]
async fn uv_stands_down_when_ruff_is_available() {
    let (root, bin, _subject) = workspace();
    let log = root.path().join("formatted.log");
    let stub = recording_stub(&bin, &log);
    std::fs::write(root.path().join("ruff.toml"), "line-length = 100\n").expect("ruff config");

    let python = root.path().join("module.py");
    std::fs::write(&python, BEFORE).expect("python fixture");
    let runtime = Formatters::new(
        root.path(),
        root.path(),
        &zuno_catalog::formatter::ResolvedFormatters::resolve(Some(&config("true"))),
    )
    .with_locator(Arc::new(
        StubPrograms::default()
            .with("ruff", &stub)
            .with("uv", &stub),
    ));

    assert!(runtime.format_all(&python).await.changed);
    let logged = std::fs::read_to_string(&log).expect("log");
    assert_eq!(
        logged.lines().count(),
        1,
        "only ruff should have run; uv shares the backend: {logged:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ruff_needs_a_config_or_a_dependency_that_names_it() {
    let (root, bin, _subject) = workspace();
    let stub = rewriting_stub(&bin);
    let python = root.path().join("module.py");
    std::fs::write(&python, BEFORE).expect("python fixture");
    // uv is isolated by the locator, not by `disabled` — disabling either of the
    // pair disables both, which is asserted separately.
    let runtime = Formatters::new(
        root.path(),
        root.path(),
        &zuno_catalog::formatter::ResolvedFormatters::resolve(Some(&config("true"))),
    )
    .with_locator(Arc::new(StubPrograms::default().with("ruff", &stub)));

    assert_eq!(
        runtime.format_all(&python).await,
        FormatOutcome::default(),
        "ruff is installed but the project does not use it"
    );

    // A bare pyproject.toml is not enough; it has to declare the tool.
    std::fs::write(
        root.path().join("pyproject.toml"),
        "[project]\nname = \"x\"\n",
    )
    .expect("bare manifest");
    assert_eq!(runtime.format_all(&python).await, FormatOutcome::default());

    std::fs::write(
        root.path().join("pyproject.toml"),
        "[project]\nname = \"x\"\n\n[tool.ruff]\nline-length = 100\n",
    )
    .expect("configured manifest");
    assert!(runtime.format_all(&python).await.changed);
}

#[cfg(unix)]
#[tokio::test]
async fn the_help_probe_distinguishes_the_r_formatter_from_a_namesake() {
    let (root, bin, _subject) = workspace();
    let impostor = script(
        &bin,
        "air-impostor",
        "#!/bin/sh\necho 'Air: a music player'\n",
    );
    let real = script(
        &bin,
        "air-real",
        &format!(
            "if [ \"$1\" = '--help' ]; then echo 'Air: An R language server and formatter'; exit 0; fi\n{LAST_ARGUMENT}printf '%s' '{AFTER}' > \"$target\"\n"
        ),
    );

    let source = root.path().join("script.R");
    let only_air = serde_json::to_string(&json!({})).expect("config json");
    let build = |program: &Path| {
        Formatters::new(root.path(), root.path(), &resolved(&only_air))
            .with_locator(Arc::new(StubPrograms::default().with("air", program)))
    };

    std::fs::write(&source, BEFORE).expect("R fixture");
    assert_eq!(
        build(&impostor).format_all(&source).await,
        FormatOutcome::default(),
        "a binary called air that is not the R formatter must be refused"
    );

    std::fs::write(&source, BEFORE).expect("R fixture");
    assert!(build(&real).format_all(&source).await.changed);
    assert_eq!(std::fs::read_to_string(&source).expect("read back"), AFTER);
}

// ---------------------------------------------------------------------------
// Reporting shape
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn the_failure_report_names_the_formatter_the_command_and_the_reason() {
    let (root, bin, subject) = workspace();
    let stub = failing_stub(&bin);
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-fail", &stub)),
    );

    let outcome = runtime.format_all(&subject).await;
    assert!(outcome.has_failures());
    let report = outcome.report();
    assert!(report.contains("Formatter `stub`"), "{report}");
    assert!(report.contains("exited with status 3"), "{report}");
    assert!(
        report.contains(subject.to_string_lossy().as_ref()),
        "the substituted path must appear in the reported command: {report}"
    );
    assert!(
        report.contains("stub-fail: cannot parse line 1: unexpected token"),
        "{report}"
    );

    let metadata = outcome.failure_metadata().expect("metadata");
    assert_eq!(metadata.as_array().map(Vec::len), Some(1));
    assert!(
        FormatOutcome::default().failure_metadata().is_none(),
        "no failures means no metadata key"
    );
}

// ---------------------------------------------------------------------------
// What a ceiling reaches, and what the environment can do to a formatter
// ---------------------------------------------------------------------------

/// Whether `pid` still names a process, reaped or not.
///
/// `ps -p` rather than `/proc/{pid}`, which exists only on Linux and would make the
/// assertions below vacuously true on macOS.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The probe itself has to be able to see a live process.
///
/// `ps` missing, or refusing, would make [`process_exists`] answer `false` for every pid
/// and every "it is gone" assertion would pass without having observed anything. This
/// process's own pid is the one case whose answer is known.
#[cfg(unix)]
fn assert_process_probe_works() {
    assert!(
        process_exists(std::process::id()),
        "`ps -p` cannot see this test process, so it cannot witness any other process \
         either: the exit assertions would be vacuous"
    );
}

#[cfg(unix)]
async fn read_pid_when_written(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(raw) = std::fs::read_to_string(path)
                && let Ok(pid) = raw.trim().parse::<u32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} was never written", path.display()))
}

#[cfg(unix)]
async fn assert_process_stopped(label: &str, pid: u32) {
    assert_process_probe_works();
    let stopped = tokio::time::timeout(Duration::from_secs(2), async {
        while process_exists(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "{label} (pid {pid}) outlived the ceiling: the abandoned formatter's process group \
         was never torn down"
    );
}

/// A formatter abandoned at the ceiling takes everything it started with it.
///
/// `kill_on_drop(true)` reaches the **direct child only**: dropping the wait future
/// `SIGKILL`s the formatter and leaves every process it started running. `process_group(0)`
/// makes that worse rather than better — the group is no longer Zuno's, so neither the
/// terminal's `SIGINT` nor Zuno's own group teardown reaches it — so a `prettier` wrapper's
/// `node` daemon, a `rustfmt` shim's `sleep`, or a formatter's language server survived
/// every edit for the rest of the login session with nothing left to reap it. Two comments
/// beside the spawn asserted the opposite.
#[cfg(unix)]
#[tokio::test]
async fn a_formatter_abandoned_at_the_ceiling_takes_its_helpers_with_it() {
    let (root, bin, subject) = workspace();
    let leader_pid = root.path().join("formatter.pid");
    let helper_pid = root.path().join("helper.pid");
    // The helper is a child of the formatter, so it joins the group `process_group(0)` gave
    // the formatter. `exec` so the leader pid is the process that outlives the ceiling.
    let stub = script(
        &bin,
        "stub-hang-group",
        &format!(
            "sleep 60 &\nprintf '%s' \"$!\" > '{helper}'\nprintf '%s' \"$$\" > '{leader}'\nexec sleep 60\n",
            helper = helper_pid.display(),
            leader = leader_pid.display()
        ),
    );
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-hang-group", &stub)),
    )
    .with_ceiling(Duration::from_millis(300));

    let leader = read_pid_when_written(&leader_pid);
    let helper = read_pid_when_written(&helper_pid);
    let outcome = runtime.format_all(&subject);
    let (leader, helper, outcome) = tokio::join!(leader, helper, outcome);

    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(
        outcome.failures[0].kind,
        FailureKind::TimedOut { after_seconds: 0 }
    );
    assert_process_stopped("the abandoned formatter", leader).await;
    assert_process_stopped("the helper the formatter spawned", helper).await;
}

/// The `#[test]` the environment test re-execs itself as.
///
/// A `--exact` filter that matches nothing is not an error to libtest: it prints
/// `ok. 0 passed; 0 failed; N filtered out` and exits 0, so a parent that asserts only
/// `status.success()` passes without a single assertion having run. Renaming the test would
/// be enough to disarm it silently, so the parent asserts this name appears in the binary's
/// own `--list` and insists on seeing [`ENVIRONMENT_OBSERVED`].
#[cfg(unix)]
const ENVIRONMENT_TEST: &str = "an_environment_entry_zuno_cannot_spell_still_formats_the_file";

/// The child's proof that it ran to the end of its assertions.
#[cfg(unix)]
const ENVIRONMENT_OBSERVED: &str = "zuno-tools: the formatter's environment was observed";

/// A byte no UTF-8 sequence can start with, which is what makes an entry unspellable.
///
/// `0xE9` is `é` in Latin-1, so this is not a synthetic hostile input: it is what a
/// `LANG=en_US.ISO-8859-1` login, a Windows console codepage, or a filename from a
/// pre-Unicode archive puts into the environment of the process that launches Zuno.
#[cfg(unix)]
const LATIN1_E_ACUTE: u8 = 0xE9;

#[cfg(unix)]
fn non_unicode(prefix: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt as _;
    let mut bytes = prefix.to_vec();
    bytes.push(LATIN1_E_ACUTE);
    std::ffi::OsString::from_vec(bytes)
}

/// An entry Zuno cannot spell costs neither the formatting run nor the turn.
///
/// `std::env::vars` **panics** on any entry whose name or value is not Unicode, and this
/// panic was not confined to the formatter: it unwound the turn. So in a process launched
/// with one Latin-1 byte anywhere in its environment — a `LANG` a distribution still ships,
/// a `PWD` under a pre-Unicode directory name — *every* post-edit format failed, and no
/// message named the reason. The decision this pins is that such an entry is **passed
/// through unchanged** rather than dropped: dropping it silently changes the environment
/// the operator's formatter runs in, and a formatter that reads it would then behave one
/// way under Zuno and another way in a terminal.
///
/// The assertions run in a child of this test binary because the entry has to be real, and
/// setting one in this process is `unsafe`, which this workspace forbids.
#[cfg(unix)]
#[tokio::test]
async fn an_environment_entry_zuno_cannot_spell_still_formats_the_file() {
    const CHILD: &str = "ZUNO_TOOLS_FORMATTER_ENVIRONMENT_CHILD";

    if std::env::var_os(CHILD).is_none() {
        let binary = std::env::current_exe().expect("this test binary");
        let listed = std::process::Command::new(&binary)
            .args(["--list", "--format", "terse"])
            .output()
            .expect("list this binary's tests");
        let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
        assert!(
            listed.contains(&format!("{ENVIRONMENT_TEST}: test")),
            "`{ENVIRONMENT_TEST}` is not a test in this binary, so the re-exec below would \
             filter to nothing and pass without asserting anything:\n{listed}"
        );

        let child = std::process::Command::new(&binary)
            .args(["--exact", ENVIRONMENT_TEST, "--nocapture"])
            .env(CHILD, "1")
            // A value Zuno cannot spell, under a name a POSIX shell still exports. This is
            // the entry the child follows all the way into the formatter.
            .env("LOCALE_PROBE", non_unicode(b"caf"))
            // The same shape inside Zuno's namespace: the withholding rule has to hold for
            // a value Zuno cannot spell, or an unspellable value would be the way around it.
            .env("ZUNO_PROBE", non_unicode(b"caf"))
            // A *name* Zuno cannot spell. `std::env::vars` panics on this one exactly as it
            // does on the values above, which is what the child's first assertion checks it
            // is carrying. It is deliberately not followed into the formatter: `/bin/sh`
            // drops an environment name that is not a valid shell identifier before `env`
            // can print it (measured: absent through `sh -c env`, present through a direct
            // `exec` of `env`), so a formatter stub written as a shell script cannot witness
            // it. What happens to it inside Zuno is pinned at unit level instead, on
            // `withhold_zuno_environment`.
            .env(non_unicode(b"LOCALE_NAME_PROBE_"), non_unicode(b"caf"))
            .output()
            .expect("re-run this test with an environment entry that is not Unicode");
        let stdout = String::from_utf8_lossy(&child.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&child.stderr).into_owned();
        assert!(
            child.status.success(),
            "the child's assertions must hold:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains(ENVIRONMENT_OBSERVED),
            "the child never reported observing the formatter's environment, so nothing was \
             asserted:\n{stdout}\n{stderr}"
        );
        return;
    }

    // Vacuity check first: on a platform or libc that refused to pass the bytes through,
    // every assertion below would hold without the hostile case ever existing.
    let unspellable: Vec<_> = std::env::vars_os()
        .filter(|(name, value)| name.to_str().is_none() || value.to_str().is_none())
        .collect();
    assert!(
        unspellable.len() >= 3,
        "this process carries {} environment entries that are not Unicode, not the three the \
         parent set, so nothing below would be tested",
        unspellable.len()
    );
    assert!(
        unspellable
            .iter()
            .any(|(name, _)| name.as_encoded_bytes().starts_with(b"LOCALE_NAME_PROBE_")),
        "the entry whose *name* is not Unicode did not survive the spawn, so the panic this \
         test exists for would not be reachable from here"
    );

    let (root, bin, subject) = workspace();
    let dump = root.path().join("formatter.env");
    let stub = script(
        &bin,
        "stub-env",
        &format!(
            "{LAST_ARGUMENT}env > '{dump}'\nprintf '%s' '{AFTER}' > \"$target\"\n",
            dump = dump.display()
        ),
    );
    let runtime = formatters(
        root.path(),
        &serde_json::to_string(&json!({
            "stub": entry(Some(command(&stub, &["$FILE"])), Some(&[".txt"]), None, None),
        }))
        .expect("config json"),
        Arc::new(StubPrograms::default().with("stub-env", &stub)),
    );

    let outcome = runtime.format_all(&subject).await;
    assert!(
        outcome.failures.is_empty(),
        "an unspellable environment entry cost the formatting run: {:?}",
        outcome.failures
    );
    assert!(outcome.changed);
    assert_eq!(
        std::fs::read_to_string(&subject).expect("read back"),
        AFTER,
        "the formatter did not actually run"
    );

    // Read the dump as bytes: a `String` would have replaced the entry with U+FFFD before
    // the assertion could see it.
    let raw = std::fs::read(&dump).expect("the formatter recorded its environment");
    let mut preserved = Vec::new();
    preserved.extend_from_slice(b"LOCALE_PROBE=caf");
    preserved.push(LATIN1_E_ACUTE);
    assert!(
        raw.windows(preserved.len())
            .any(|window| window == preserved),
        "the entry Zuno cannot spell did not reach the formatter unchanged:\n{}",
        String::from_utf8_lossy(&raw)
    );
    assert!(
        !raw.windows(b"ZUNO_PROBE".len())
            .any(|window| window == b"ZUNO_PROBE"),
        "a withheld name reached the formatter because its value was not Unicode:\n{}",
        String::from_utf8_lossy(&raw)
    );
    println!("{ENVIRONMENT_OBSERVED}");
}

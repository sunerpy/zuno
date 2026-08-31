//! Whether the diagnostics this binary ships are actually *reachable*.
//!
//! # Why a whole test file for one wiring
//!
//! `zuno-observability`'s memory subsystem shipped complete and unreachable: four
//! attribution levels, thresholds calibrated against measured sessions, fifteen passing
//! unit tests — and no production construction of a `MemoryRing` and no production call
//! to `observe` or `report` anywhere in the workspace. The alert could not fire however
//! far resident memory grew, and the whole suite was green.
//!
//! `crates/zuno-observability/tests/memory.rs` proves the sampler *works*. Nothing there
//! can prove this binary *starts* one — which is precisely the half that was missing, and
//! precisely the half a green suite did not notice. So this file asserts the wiring: that
//! the long-running commands are classified as such, and that `run_process` really
//! consults that classification and spawns a sampler.
//!
//! # Why source evidence rather than running the command
//!
//! The commands that get a sampler are `tui` and `serve`, and neither returns: one waits
//! on a key press and the other on an inbound request. A test that launched either to see
//! whether a thread appeared would be a test that hangs. The repository already gates a
//! reachability claim this way — `zuno-testkit/tests/backpressure.rs` pairs every channel
//! with a source needle at its real call site, and that gate caught an unregistered
//! channel red while clippy, fmt and deny were all green.

use std::path::{Path, PathBuf};

use clap::Parser as _;
use zuno_cli::{Action, Cli};

/// The classification `run_process` would reach for this argv.
///
/// Resolved through the real parse rather than by constructing the variant, so the answer
/// is the one the binary gets — the same reason `surface.rs` builds its requests this way.
fn deserves_a_sampler(argv: &[&str]) -> bool {
    let cli = Cli::try_parse_from(std::iter::once("zuno").chain(argv.iter().copied()))
        .unwrap_or_else(|error| panic!("{argv:?} must parse: {error}"));
    match cli.action(&zuno_paths::Env::empty()) {
        Action::Dispatch(request) => request.args.deserves_a_memory_sampler(),
        other => panic!("{argv:?} must dispatch, got {other:?}"),
    }
}

fn cli_source(relative: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Collapse whitespace so an assertion survives `cargo fmt` rewrapping a call.
fn compact(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove whitespace entirely when the assertion is about one Rust expression.
fn squash_whitespace(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// Every production Rust source in the workspace, tests excluded.
///
/// Tests are excluded on purpose: a setter only a test calls is exactly the state this
/// file exists to reject, so counting test callers would make the gate agree with itself.
/// The same exclusion `zuno-testkit/tests/backpressure.rs` applies for the same reason.
fn production_sources() -> Vec<String> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("zuno-cli is inside crates");
    let mut sources = Vec::new();
    let mut stack = vec![crates.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries {
            let path = entry.expect("directory entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if path.is_dir() {
                if name != "tests" && name != "target" && name != "benches" {
                    stack.push(path);
                }
                continue;
            }
            if path.extension().is_some_and(|extension| extension == "rs")
                && !name.ends_with("_tests.rs")
            {
                sources.push(std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    sources
}

#[test]
fn only_the_commands_that_outlive_a_turn_are_given_a_memory_sampler() {
    // Growth means nothing over a command that prints and exits, and a sampler thread
    // started for one would be pure startup cost on the path G1 budgets.
    assert!(
        deserves_a_sampler(&["tui"]),
        "the TUI runs until the user leaves"
    );
    assert!(
        deserves_a_sampler(&["serve"]),
        "a server runs until it is stopped"
    );
    for argv in [
        vec!["models"],
        vec!["providers", "list"],
        vec!["session", "list"],
        vec!["agent", "list"],
    ] {
        assert!(
            !deserves_a_sampler(&argv),
            "{argv:?} prints and exits, so a sampler thread would be pure startup cost \
             on the path the G1 budget measures"
        );
    }
}

#[test]
fn the_process_entry_point_starts_a_memory_sampler_for_a_long_running_command() {
    // The assertion that would have caught the original defect. Each needle is a real
    // fragment of `run_process`, so deleting the wiring — or reducing it to a construction
    // that is never spawned — fails here.
    let source = compact(&cli_source("src/lib.rs"));
    for needle in [
        "request.args.deserves_a_memory_sampler()",
        "zuno_observability::memory::MemorySampler::spawn(",
        "zuno_observability::memory::active_sessions()",
        "memory.shutdown();",
    ] {
        assert!(
            source.contains(&compact(needle)),
            "`run_process` has no evidence of {needle}; the memory alert is unreachable \
             again, which is the defect this file exists to prevent"
        );
    }
}

#[test]
fn headless_turn_progress_reaches_the_liveness_watchdog() {
    // A working renderer alone is not enough: the original false-positive happened
    // because `run_process` held a busy guard while the turn's real events had no route
    // back to it. Lock all three production seams so the callback cannot become another
    // fully tested but unreachable diagnostic.
    let entry = compact(&cli_source("src/lib.rs"));
    for needle in [
        "let progress = || watchdog.beat(dispatch_phase);",
        "HeadlessCommandDispatcher::new(progress)",
    ] {
        assert!(
            entry.contains(&compact(needle)),
            "the process entry point no longer wires `{needle}` into dispatch"
        );
    }

    let dispatcher = compact(&cli_source("src/cmd/mod.rs"));
    assert!(
        dispatcher.contains(&compact(
            "run::execute(args, &request.environment, self.progress)"
        )),
        "the dispatcher no longer forwards watchdog progress to the headless turn"
    );

    let run = compact(&cli_source("src/cmd/run.rs"));
    for needle in [
        "render_events(receiver, args.format, args.show_reasoning, progress)",
        "while let Some(event) = receiver.recv().await { report_progress(progress);",
    ] {
        assert!(
            run.contains(&compact(needle)),
            "headless turn events no longer drive `{needle}`"
        );
    }
}

#[test]
fn the_standalone_server_binary_starts_one_too() {
    // Two long-lived entry points, so two wirings: `zuno serve` goes through the CLI's
    // `run_process`, and `zuno-server` has its own `main`. Wiring only the first is how one
    // of them silently keeps the hole.
    let source = compact(&cli_source("../zuno-server/src/main.rs"));
    assert!(
        source.contains(&compact("MemorySampler::spawn(")),
        "the standalone server binary starts no memory sampler"
    );
    assert!(
        source.contains(&compact("memory.shutdown();")),
        "the standalone server binary never stops its sampler"
    );
}

#[test]
fn the_status_strip_has_no_setter_that_nothing_in_production_ever_calls() {
    // The defect this catches cannot be caught by a behavioural test, which is why it
    // survived: `StatusView::set_cost` existed, was tested, had a comment naming
    // `zuno-cli/src/cmd/tui.rs` as its caller — and no such call existed. Nothing rendered
    // a price in any real session, and every strip test passed either way, because a field
    // nobody sets is invisible to a test that does not set it.
    //
    // So the assertion has to be about reachability rather than output: a setter on the
    // one row always on screen must have a caller outside the tests, or it is advertising
    // a segment that can never be filled.
    /// Setters this gate knowingly allows, and why.
    ///
    /// Listed rather than silently skipped, the way `backpressure.rs` lists its excluded
    /// channels: an unexplained skip is how the next unreachable setter gets absorbed.
    const ALLOWED: &[(&str, &str)] = &[(
        "set_context",
        "live re-theming reaches the strip through the shared `ViewContext` the picker \
         mutates (`ViewContext::set_theme`), so this setter is redundant rather than a \
         segment nobody fills. It is a separate finding from the cost segment and is left \
         for whoever traces the theme path, not deleted here on a guess.",
    )];

    let view_source = cli_source("../zuno-tui/src/views/message.rs");
    let mut unreachable = Vec::new();
    for line in view_source.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub fn set_") else {
            continue;
        };
        let Some(name) = rest.split('(').next() else {
            continue;
        };
        if ALLOWED
            .iter()
            .any(|(allowed, _)| *allowed == format!("set_{name}"))
        {
            continue;
        }
        let setter = format!(".set_{name}(");
        if !production_sources().iter().any(|source| {
            source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .any(|line| line.contains(&setter))
        }) {
            unreachable.push(format!("set_{name}"));
        }
    }
    assert!(
        unreachable.is_empty(),
        "these `StatusView` setters have no production caller, so the segments they fill \
         are permanently empty while the code advertises them: {unreachable:?}. Either \
         wire one, or remove it — an empty segment claiming to be a value is worse than \
         no segment."
    );
}

/// Every `fn` in `source`, as `(name, body)`.
///
/// Bodies are cut by brace depth from the signature's opening `{`, so a nested `fn`,
/// closure or `match` cannot leak a neighbour's contents into the answer. That precision
/// is the whole point here: a per-*file* search is what let the defect through.
fn functions(source: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        let Some(after) = trimmed
            .strip_prefix("fn ")
            .or_else(|| trimmed.strip_prefix("async fn "))
            .or_else(|| trimmed.strip_prefix("pub fn "))
            .or_else(|| trimmed.strip_prefix("pub async fn "))
            .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
            .or_else(|| trimmed.strip_prefix("pub(crate) async fn "))
            .or_else(|| trimmed.strip_prefix("pub(super) fn "))
            .or_else(|| trimmed.strip_prefix("pub(super) async fn "))
        else {
            index += 1;
            continue;
        };
        let name = after
            .split(['(', '<', ' '])
            .next()
            .unwrap_or_default()
            .to_owned();
        // Walk to the signature's opening brace, which may be lines below on a wrapped
        // signature, then to its match.
        let mut depth = 0_i32;
        let mut opened = false;
        let mut body = String::new();
        let mut cursor = index;
        while cursor < lines.len() {
            for character in lines[cursor].chars() {
                match character {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if opened {
                body.push_str(lines[cursor]);
                body.push('\n');
                if depth <= 0 {
                    break;
                }
            }
            cursor += 1;
        }
        found.push((name, body));
        index = cursor.max(index + 1);
    }
    found
}

#[test]
fn every_place_that_admits_a_turn_also_counts_the_session() {
    // `Attribution::SessionCount` is the level that tells a busy server from a leaking one,
    // and it is meaningless unless something maintains the count.
    //
    // Prompt admission starts one durable driver. That driver retains one count while it
    // drains the FIFO, including the later leases `continue_prompt_driver` obtains.
    // Compaction owns one synchronous lease and therefore owns its count directly.
    const ADMISSION: &str = "services.runs.begin_turn(";
    let server = cli_source("../zuno-server/src/api/session.rs");
    let functions = functions(&server);
    let mut admitting = functions
        .iter()
        .filter(|(_, body)| squash_whitespace(body).contains(ADMISSION))
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    admitting.sort_unstable();
    assert_eq!(
        admitting,
        ["compact_session", "continue_prompt_driver", "prompt"],
        "every new server run admission must declare which session-count lifetime owns it"
    );

    let function = |name: &str| {
        functions
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, body)| body)
            .unwrap_or_else(|| panic!("server session API has no `{name}` function"))
    };
    let prompt = function("prompt");
    assert!(
        squash_whitespace(prompt).contains("spawn_prompt_driver("),
        "`prompt` admits the first lease but does not hand it to the counted durable driver"
    );
    let driver = function("spawn_prompt_driver");
    assert!(
        compact(driver).contains(&compact(
            "zuno_observability::memory::SessionCount::enter()"
        )),
        "the durable prompt driver drains live leases without counting its session"
    );
    assert!(
        squash_whitespace(driver).contains("continue_prompt_driver("),
        "the counted prompt driver no longer owns the FIFO continuation admissions"
    );

    let compact_session = function("compact_session");
    assert!(
        compact(compact_session).contains(&compact(
            "zuno_observability::memory::SessionCount::enter()"
        )),
        "`compact_session` admits a live turn without counting the session"
    );
}

#[test]
fn the_tui_turn_driver_counts_its_session_inside_the_function_that_drives_it() {
    // The TUI has one place a turn runs, and the guard has to be *in* it: a guard anywhere
    // else in the file would be dropped at the wrong time, so the count would not describe
    // what is actually in flight.
    let tui = cli_source("src/cmd/tui.rs");
    let (_, body) = functions(&tui)
        .into_iter()
        .find(|(name, _)| name == "drive_one")
        .expect("the TUI drives a turn in `drive_one`");
    assert!(
        compact(&body).contains(&compact(
            "zuno_observability::memory::SessionCount::enter()"
        )),
        "`drive_one` does not count its session, so the sampler's session attribution \
         would read zero however many turns have run"
    );
}

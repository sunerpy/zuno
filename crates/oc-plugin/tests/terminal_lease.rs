//! The plugin host's half of the terminal-lease protocol, proven without a TUI.
//!
//! A plugin's compat host is a `bun`/`node` child that prompts on stdin
//! (kiro@0.18.0's `node:readline/promises` OAuth flow), while the Rust TUI holds the
//! same TTY in raw mode. Blocker B7 is that the original acceptance criterion for the
//! host wanted this proven against "a running TUI" that does not exist for another
//! thirteen todos. The lease is the seam that makes it provable now: the host only
//! ever asks for a lease, so its side can be verified against
//! `oc_testkit::FakeTerminalOwner`, and the real ratatui/pty integration stays todo
//! 73's problem.
//!
//! Every test here runs with no TTY, no pty, and no `oc-tui` in the dependency graph.
//! [`terminal_lease_keeps_the_plugin_crate_away_from_the_tui_and_ratatui`] asserts that last part
//! mechanically, so a future dependency addition breaks CI rather than silently
//! recreating the inverted dependency this whole protocol exists to avoid.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oc_engine::terminal_lease::{
    DEFAULT_LEASE_TIMEOUT, LeaseReason, TerminalBroker, TerminalLease, TerminalLeaseError,
};
use oc_testkit::{FakeTerminalOwner, TerminalTransition};

/// A deadline no test run can reach.
///
/// Used wherever the assertion is that a force-reclaim did *not* happen. Load can
/// only make a timer fire late, so this direction cannot flake.
const NEVER: Duration = Duration::from_secs(3_600);

/// A deadline that has already passed by the time `acquire` returns.
///
/// Zero rather than "a few milliseconds": the sweep in `acquire` compares against
/// `Instant::now`, so an elapsed deadline is a fact rather than a race, and the test
/// needs no sleeping at all.
const ALREADY_EXPIRED: Duration = Duration::ZERO;

/// Long enough that a loaded machine's late timer is still observed, short enough
/// that a genuine failure to fire is caught in seconds.
///
/// Paired with a millisecond lease deadline, so what is being waited on is an
/// *observation*, not a production interval.
const OBSERVE_BUDGET: Duration = Duration::from_secs(10);

fn broker(owner: &Arc<FakeTerminalOwner>, timeout: Duration) -> TerminalBroker {
    TerminalBroker::with_timeout(Arc::clone(owner) as Arc<_>, timeout)
}

/// Stands in for the `bun`/`node` compat host's `node:readline/promises` prompt.
///
/// It reads from a scripted queue rather than stdin on purpose: reading real stdin
/// would need a TTY, and the thing under test is the *ownership handoff*, not
/// line editing. The lease must be held across the whole exchange, which is what
/// taking `&TerminalLeaseGuard` enforces at the type level.
fn fake_interactive_prompt(
    guard: &oc_engine::terminal_lease::TerminalLeaseGuard,
    scripted: &mut VecDeque<String>,
) -> Option<String> {
    assert!(
        !guard.was_reclaimed(),
        "a host must not prompt on a terminal the deadline already took back"
    );
    scripted.pop_front()
}

#[tokio::test]
async fn terminal_lease_acquire_and_release_are_observed_in_order() {
    let owner = Arc::new(FakeTerminalOwner::new());
    let transcript = owner.transcript();
    let broker = broker(&owner, NEVER);

    let guard = broker
        .acquire(LeaseReason::new("kiro", "device-code prompt"))
        .await
        .expect("a vacant terminal must be grantable");

    assert_eq!(
        transcript.transitions(),
        vec![TerminalTransition::Acquired {
            plugin: "kiro".to_owned(),
            purpose: "device-code prompt".to_owned(),
        }],
        "the owner must have yielded before the guard existed"
    );

    guard.release();

    assert_eq!(
        transcript.transitions(),
        vec![
            TerminalTransition::Acquired {
                plugin: "kiro".to_owned(),
                purpose: "device-code prompt".to_owned(),
            },
            TerminalTransition::Released {
                plugin: "kiro".to_owned()
            },
        ],
        "release must follow acquire, and must not be a force-reclaim"
    );
    assert_eq!(transcript.forced_count(), 0);
}

/// The `Drop` release is the whole reason a host cannot forget to give the terminal
/// back: an early `return`, a `?`, or a panic all still release it.
#[tokio::test]
async fn terminal_lease_releases_when_the_guard_goes_out_of_scope() {
    let owner = Arc::new(FakeTerminalOwner::new());
    let transcript = owner.transcript();
    let broker = broker(&owner, NEVER);

    {
        let _guard = broker
            .acquire(LeaseReason::new("kiro", "api key prompt"))
            .await
            .expect("acquire");
        assert!(broker.is_held());
    }

    assert!(
        !broker.is_held(),
        "leaving the scope must return the terminal"
    );
    assert!(transcript.released_by("kiro"));
    assert_eq!(transcript.ownership_changes(), 2);
}

/// QA happy path: the compat host acquires, runs an interactive prompt, and releases,
/// with the fake owner observing both transitions.
#[tokio::test]
async fn terminal_lease_happy_path_runs_a_fake_prompt_between_both_transitions() {
    let owner = Arc::new(FakeTerminalOwner::new());
    let transcript = owner.transcript();
    let broker = broker(&owner, NEVER);
    let mut scripted = VecDeque::from(["WDJB-MJHT".to_owned()]);

    let typed = {
        let guard = broker
            .acquire(LeaseReason::new("kiro", "enter the device code"))
            .await
            .expect("the host must be given the terminal to prompt on");

        assert!(
            transcript.acquired_by("kiro"),
            "the terminal must be yielded before the prompt reads a line"
        );
        assert!(
            !transcript.released_by("kiro"),
            "the terminal must still be the host's while the prompt is open"
        );

        fake_interactive_prompt(&guard, &mut scripted)
    };

    assert_eq!(typed.as_deref(), Some("WDJB-MJHT"));
    assert!(
        transcript.released_by("kiro"),
        "the terminal must come back once the prompt is done"
    );
    assert_eq!(transcript.forced_count(), 0);
    assert_eq!(owner.yields_requested(), 1);
}

/// QA failure path, driven by the deadline sweep rather than a timer: fully
/// deterministic, no sleeping, and it still proves the diagnostic names the plugin.
#[tokio::test]
async fn terminal_lease_force_reclaims_a_host_that_never_releases() {
    let owner = Arc::new(FakeTerminalOwner::new());
    let transcript = owner.transcript();
    let broker = broker(&owner, ALREADY_EXPIRED);

    let never_released = broker
        .acquire(LeaseReason::new("kiro", "device-code prompt"))
        .await
        .expect("acquire");

    let diagnostic = broker
        .reclaim_if_expired()
        .expect("a lease held past its deadline must be reclaimed");

    assert_eq!(diagnostic.plugin, "kiro");
    assert_eq!(diagnostic.timeout, ALREADY_EXPIRED);
    let rendered = diagnostic.to_string();
    assert!(
        rendered.contains("plugin `kiro`"),
        "the diagnostic must name the plugin: {rendered}"
    );
    assert!(
        rendered.contains("did not release it"),
        "the diagnostic must say what went wrong: {rendered}"
    );

    let observed = transcript
        .forced_diagnostic("kiro")
        .expect("the owner must have been told the reclaim was forced");
    assert_eq!(observed, rendered);
    assert!(
        never_released.was_reclaimed(),
        "the host must be able to see that its lease was taken away"
    );

    // Dropping the wedged guard afterwards must not reclaim a second time: by then the
    // terminal may belong to someone else, and restoring it again would corrupt them.
    drop(never_released);
    assert_eq!(transcript.forced_count(), 1);
    assert!(!transcript.released_by("kiro"));
}

/// The same failure, driven by the watchdog rather than an explicit sweep. This is the
/// path that matters in production, because nothing else may ever look.
#[tokio::test]
async fn terminal_lease_watchdog_reclaims_without_anyone_asking() {
    let owner = Arc::new(FakeTerminalOwner::new());
    let transcript = owner.transcript();
    let broker = broker(&owner, Duration::from_millis(5));

    let _never_released = broker
        .acquire(LeaseReason::new("kiro", "device-code prompt"))
        .await
        .expect("acquire");

    let diagnostic = transcript
        .wait_for_forced("kiro", OBSERVE_BUDGET)
        .await
        .expect("the watchdog must reclaim a lease nobody released");

    assert!(
        diagnostic.contains("plugin `kiro`"),
        "the diagnostic must name the plugin: {diagnostic}"
    );
    assert!(
        !broker.is_held(),
        "a force-reclaimed lease must free the terminal"
    );
}

/// A leaked guard must not wedge the terminal for the rest of the session, which is
/// what the sweep inside `acquire` buys over relying on the watchdog alone.
#[tokio::test]
async fn terminal_lease_expired_holder_does_not_block_the_next_host() {
    let owner = Arc::new(FakeTerminalOwner::new());
    let transcript = owner.transcript();
    let broker = broker(&owner, ALREADY_EXPIRED);

    let leaked = broker
        .acquire(LeaseReason::new("kiro", "device-code prompt"))
        .await
        .expect("first acquire");

    let second = broker
        .acquire(LeaseReason::new("other-plugin", "api key prompt"))
        .await
        .expect("an expired holder must not refuse the next host");

    assert!(transcript.forced_diagnostic("kiro").is_some());
    assert!(transcript.acquired_by("other-plugin"));
    drop(leaked);
    drop(second);
    assert!(transcript.released_by("other-plugin"));
    assert_eq!(transcript.forced_count(), 1);
}

/// The declared concurrent-acquire policy: refusal, naming the holder. Queueing would
/// turn a wedged host back into a hang, and preemption would yank the terminal out
/// from under half-typed input.
#[tokio::test]
async fn terminal_lease_second_concurrent_acquire_is_refused_and_names_the_holder() {
    let owner = Arc::new(FakeTerminalOwner::new());
    let transcript = owner.transcript();
    let broker = broker(&owner, NEVER);

    let first = broker
        .acquire(LeaseReason::new("kiro", "device-code prompt"))
        .await
        .expect("first acquire");

    let error = broker
        .acquire(LeaseReason::new("other-plugin", "api key prompt"))
        .await
        .expect_err("the declared policy is refusal, not queueing or preemption");

    assert_eq!(
        error,
        TerminalLeaseError::Busy {
            holder: "kiro".to_owned(),
            holder_purpose: "device-code prompt".to_owned(),
            requested_by: "other-plugin".to_owned(),
        }
    );
    let rendered = error.to_string();
    assert!(rendered.contains("held by plugin `kiro`"), "{rendered}");
    assert!(
        rendered.contains("`other-plugin` cannot prompt"),
        "{rendered}"
    );

    // Refusal must be inert: the holder keeps the terminal, and the owner is not asked
    // to yield a terminal it has already yielded.
    assert_eq!(owner.yields_requested(), 1);
    assert_eq!(transcript.ownership_changes(), 1);
    assert!(broker.is_held());
    assert_eq!(broker.holder().as_deref(), Some("kiro"));

    drop(first);
    broker
        .acquire(LeaseReason::new("other-plugin", "api key prompt"))
        .await
        .expect("the refusal must be about timing, not about the plugin");
    assert_eq!(transcript.forced_count(), 0);
}

/// Refusal must hold when the two acquires really are concurrent, not merely
/// sequential. Exactly one wins; the loser is refused, never queued and never granted
/// a second simultaneous lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_lease_races_grant_exactly_one_holder() {
    for round in 0..64 {
        let owner = Arc::new(FakeTerminalOwner::new());
        let transcript = owner.transcript();
        let broker = Arc::new(broker(&owner, NEVER));

        let mut contenders = Vec::new();
        for index in 0..4 {
            let broker = Arc::clone(&broker);
            contenders.push(tokio::spawn(async move {
                broker
                    .acquire(LeaseReason::new(format!("plugin-{index}"), "prompt"))
                    .await
            }));
        }

        let mut granted = Vec::new();
        let mut refused = 0;
        for contender in contenders {
            match contender.await.expect("no contender may panic") {
                Ok(guard) => granted.push(guard),
                Err(TerminalLeaseError::Busy { .. }) => refused += 1,
                Err(other) => panic!("round {round}: unexpected refusal {other}"),
            }
        }

        assert_eq!(
            granted.len(),
            1,
            "round {round}: exactly one contender may hold the terminal"
        );
        assert_eq!(refused, 3, "round {round}: the rest must be refused");
        assert_eq!(
            transcript.forced_count(),
            0,
            "round {round}: contention is not a deadline breach"
        );
        // Every yield is matched: the winner's is still open, and any contender that
        // yielded and then lost the slot handed the terminal straight back.
        assert_eq!(
            transcript.ownership_changes() % 2,
            1,
            "round {round}: one lease is still out, so the count must be odd"
        );
        drop(granted);
        assert!(
            !broker.is_held(),
            "round {round}: the terminal must come back"
        );
    }
}

/// A session with no TTY is a different answer from "someone else has it": the host
/// must not prompt either way, but only one of them is worth retrying.
#[tokio::test]
async fn terminal_lease_unavailable_terminal_is_not_reported_as_busy() {
    let owner = Arc::new(FakeTerminalOwner::refusing("no tty on this stdio session"));
    let transcript = owner.transcript();
    let broker = broker(&owner, NEVER);

    let error = broker
        .acquire(LeaseReason::new("kiro", "device-code prompt"))
        .await
        .expect_err("an owner that cannot yield must refuse the lease");

    match &error {
        TerminalLeaseError::Unavailable {
            requested_by,
            detail,
        } => {
            assert_eq!(requested_by, "kiro");
            assert!(detail.contains("no tty"), "{detail}");
        }
        other => panic!("expected Unavailable, got {other}"),
    }
    assert!(error.to_string().contains("plugin `kiro`"));
    assert!(
        !broker.is_held(),
        "a refused lease must not occupy the slot"
    );
    assert_eq!(transcript.ownership_changes(), 0);
    assert_eq!(
        transcript.transitions(),
        vec![TerminalTransition::Refused {
            plugin: "kiro".to_owned(),
            detail: "no tty on this stdio session".to_owned(),
        }]
    );
}

/// A host may hold the terminal for as long as a human takes to read a code off a
/// browser and type it back. A deadline shorter than that would reclaim mid-prompt.
#[test]
fn terminal_lease_default_deadline_outlasts_a_human_typing_a_code() {
    assert_eq!(DEFAULT_LEASE_TIMEOUT, Duration::from_secs(300));
    let owner: Arc<FakeTerminalOwner> = Arc::new(FakeTerminalOwner::new());
    assert_eq!(
        TerminalBroker::new(owner as Arc<_>).timeout(),
        DEFAULT_LEASE_TIMEOUT
    );
}

// ---------------------------------------------------------------------------
// The structural half of the acceptance criterion.
//
// `cargo tree -p oc-plugin` showing no `oc-tui` and no `ratatui` is a fact about
// today's manifests, and a fact nobody re-checks is a fact that expires. Asserting it
// here makes the inverted dependency a failing test instead of a silent regression:
// the moment someone adds `oc-tui` to this crate — or to anything it reaches — CI
// says so and names the path.
//
// Reading the manifests rather than shelling out to `cargo tree` is deliberate: a
// test that spawns cargo inside a cargo test run can deadlock on the build directory
// lock, which in a shared-target-dir workspace is a real and load-dependent hazard.
// The manifests are the input `cargo tree` reads anyway.
// ---------------------------------------------------------------------------

/// Packages that can only exist to draw a terminal interface.
///
/// `crossterm` is listed with the other two because a host that reaches it has the
/// capability to seize the terminal directly, which is exactly what the lease exists
/// to route through a protocol.
const TUI_PACKAGES: &[&str] = &["oc-tui", "ratatui", "crossterm"];

fn workspace_crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/oc-plugin has a parent")
        .to_path_buf()
}

/// The first-party dependencies each workspace crate declares, runtime and dev.
///
/// Dev-dependencies are included on purpose: a test that pulled the TUI in would
/// recreate the inverted dependency just as effectively as a runtime edge, and it is
/// the easier mistake to make.
fn first_party_graph() -> BTreeMap<String, BTreeSet<String>> {
    let crates_dir = workspace_crates_dir();
    let mut graph = BTreeMap::new();
    let entries = std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", crates_dir.display()));
    for entry in entries.filter_map(std::result::Result::ok) {
        let manifest = entry.path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let (name, deps) = parse_manifest(&manifest);
        graph.insert(name, deps);
    }
    graph
}

fn parse_manifest(path: &Path) -> (String, BTreeSet<String>) {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let manifest: toml::Value =
        toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    let name = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("{} has no package name", path.display()))
        .to_owned();

    let mut deps = BTreeSet::new();
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(toml::Value::Table(entries)) = manifest.get(table) {
            deps.extend(entries.keys().cloned());
        }
    }
    (name, deps)
}

/// Everything `root` can reach, following only first-party edges.
///
/// Third-party names are recorded but not followed: this workspace has no path from a
/// third-party crate back into a first-party one, and `ratatui` reachable *through*
/// `oc-tui` is the same defect as `ratatui` declared directly.
fn reachable_from(graph: &BTreeMap<String, BTreeSet<String>>, root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([root.to_owned()]);
    while let Some(current) = queue.pop_front() {
        let Some(deps) = graph.get(&current) else {
            continue;
        };
        for dep in deps {
            if seen.insert(dep.clone()) && graph.contains_key(dep) {
                queue.push_back(dep.clone());
            }
        }
    }
    seen
}

/// The mechanical form of `cargo tree -p oc-plugin` showing no `oc-tui`, no `ratatui`.
///
/// The floor assertions matter as much as the exclusion: a scan that found no crates,
/// or a graph in which the TUI packages do not exist at all, would pass vacuously and
/// prove nothing. Both are asserted, so a wrong-directory or renamed-crate failure is
/// loud rather than green.
#[test]
fn terminal_lease_keeps_the_plugin_crate_away_from_the_tui_and_ratatui() {
    let graph = first_party_graph();
    assert!(
        graph.len() >= 33,
        "scanned only {} crates under {}; the scan is looking in the wrong place \
         and would pass vacuously",
        graph.len(),
        workspace_crates_dir().display()
    );
    assert!(
        graph.contains_key("oc-plugin") && graph.contains_key("oc-tui"),
        "both oc-plugin and oc-tui must be in the scan for the exclusion to mean anything"
    );

    let tui_closure = reachable_from(&graph, "oc-tui");
    for package in TUI_PACKAGES.iter().filter(|name| **name != "oc-tui") {
        assert!(
            tui_closure.contains(*package),
            "{package} is not reachable from oc-tui, so excluding it from oc-plugin \
             proves nothing; the render stack was renamed or moved"
        );
    }

    let plugin_closure = reachable_from(&graph, "oc-plugin");
    let offenders: Vec<&&str> = TUI_PACKAGES
        .iter()
        .filter(|package| plugin_closure.contains(**package))
        .collect();
    assert!(
        offenders.is_empty(),
        "oc-plugin can reach {offenders:?}.\n\
         The terminal-lease protocol exists so the plugin host asks for the terminal \
         through oc_engine::terminal_lease instead of depending on the TUI. An edge \
         from oc-plugin to the render stack recreates the inverted dependency the \
         protocol was built to remove. Implement TerminalOwner in oc-tui and pass \
         `Arc<dyn TerminalOwner>` down instead."
    );
}

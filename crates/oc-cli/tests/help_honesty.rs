//! Help text may not advertise a capability the handler refuses.
//!
//! `completion` was listed in [`oc_cli::PENDING_COMMANDS`] with an honest reason
//! while `--help` still described it as "Generate shell completion output" in both
//! the root command listing and the command's own help. Every disposition test
//! passed, because each one compared the table against the dispatch arm and neither
//! reads the help a user reads first. A reader who followed that help got zero bytes
//! and exit 1 from every plausible invocation.
//!
//! The checks here are therefore written against the *rendered* help of every
//! pending command rather than against `completion` specifically, so the next
//! command added to the roster inherits them instead of repeating the defect.

use std::process::Command;

use clap::CommandFactory as _;
use oc_cli::{Cli, Disposition, PENDING_COMMANDS, disposition_for, pending_reason};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_opencode-rust"))
}

/// Phrases that tell a reader the command does not work.
///
/// Deliberately short and unambiguous: a longer list would start accepting text
/// that merely *sounds* cautious. Each entry states inability rather than caveat,
/// so a help line containing one cannot be read as a promise of output.
const UNAVAILABILITY_MARKERS: &[&str] = &["unavailable", "not available", "cannot", "unsupported"];

/// Verbs that open a promise of work performed.
///
/// This is the shape of the defect rather than its wording: the old line opened
/// with "Generate", and a reader reasonably concluded the command generates. A
/// pending command's help must open by describing what it *explains*, so a future
/// "Print the …" or "Export the …" on a stub fails here too.
const CAPABILITY_PROMISE_VERBS: &[&str] = &[
    "add",
    "build",
    "create",
    "delete",
    "display",
    "download",
    "emit",
    "export",
    "fetch",
    "generate",
    "import",
    "install",
    "launch",
    "list",
    "manage",
    "open",
    "output",
    "print",
    "produce",
    "remove",
    "render",
    "run",
    "serve",
    "set",
    "show",
    "start",
    "uninstall",
    "upgrade",
    "upload",
    "write",
];

fn states_unavailability(text: &str) -> bool {
    let lowered = text.to_lowercase();
    UNAVAILABILITY_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

fn opening_capability_promise(text: &str) -> Option<&'static str> {
    let opening = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|character: char| !character.is_alphanumeric())
        .to_lowercase();
    CAPABILITY_PROMISE_VERBS
        .iter()
        .copied()
        .find(|verb| *verb == opening)
}

/// Collapse whitespace so a claim can be found in help clap has line-wrapped.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The one-line description clap renders in the root command listing.
fn root_listing_claim(command: &str) -> String {
    Cli::command()
        .find_subcommand(command)
        .unwrap_or_else(|| panic!("`{command}` must be registered to have help"))
        .get_about()
        .unwrap_or_else(|| panic!("`{command}` must carry a description"))
        .to_string()
}

/// The full description clap renders for `<command> --help`.
fn own_help_claim(command: &str) -> String {
    let subcommand = Cli::command()
        .find_subcommand(command)
        .unwrap_or_else(|| panic!("`{command}` must be registered to have help"))
        .clone();
    subcommand
        .get_long_about()
        .or_else(|| subcommand.get_about())
        .unwrap_or_else(|| panic!("`{command}` must carry a description"))
        .to_string()
}

/// **No pending command may describe itself as doing the thing it refuses to do.**
///
/// Both surfaces are checked because the defect appeared on both: the root listing
/// and the command's own help each promised completion output independently.
#[test]
fn help_no_pending_command_advertises_a_capability_it_refuses() {
    let mut promises = Vec::new();
    for (command, _) in PENDING_COMMANDS {
        for (surface, claim) in [
            ("the root command listing", root_listing_claim(command)),
            ("its own `--help`", own_help_claim(command)),
        ] {
            if !states_unavailability(&claim) {
                promises.push(format!(
                    "`{command}` is pending, but {surface} never says so: {claim:?}"
                ));
            }
            if let Some(verb) = opening_capability_promise(&claim) {
                promises.push(format!(
                    "`{command}` is pending, but {surface} opens with the capability verb \
                     \"{verb}\": {claim:?}"
                ));
            }
        }
    }
    assert!(promises.is_empty(), "{}", promises.join("\n"));
}

/// A deliberately rejected command may not open with a capability promise either.
///
/// Their wording is already honest, so this costs nothing today and closes the
/// cheaper half of the same hole: a rejected command is registered and reachable,
/// so its listing is read exactly like a working one's.
#[test]
fn help_no_rejected_command_opens_with_a_capability_promise() {
    let mut promises = Vec::new();
    for subcommand in Cli::command().get_subcommands() {
        let name = subcommand.get_name();
        if disposition_for(name).is_none_or(|entry| entry.disposition != Disposition::Rejected) {
            continue;
        }
        let claim = root_listing_claim(name);
        if let Some(verb) = opening_capability_promise(&claim) {
            promises.push(format!(
                "`{name}` is rejected, but its listing opens with the capability verb \
                 \"{verb}\": {claim:?}"
            ));
        }
    }
    assert!(promises.is_empty(), "{}", promises.join("\n"));
}

/// The claims checked above are the ones the binary actually prints.
///
/// Reading `get_about` alone would prove a struct field, not a user-visible line;
/// this pins the rendered output so a hidden or overridden description cannot
/// satisfy the checks while the terminal still shows something else.
#[test]
fn help_the_checked_claims_are_the_rendered_ones() {
    let root = binary().arg("--help").output().expect("run --help");
    assert!(root.status.success(), "`--help` must exit 0");
    let root_help = normalized(&String::from_utf8_lossy(&root.stdout));

    for (command, _) in PENDING_COMMANDS {
        assert!(
            root_help.contains(&normalized(&root_listing_claim(command))),
            "the root listing for `{command}` is not what `--help` prints: {root_help}"
        );

        let own = binary()
            .args([command, "--help"])
            .output()
            .unwrap_or_else(|error| panic!("run `{command} --help`: {error}"));
        assert!(
            own.status.success(),
            "`{command} --help` must exit 0 so the explanation is readable"
        );
        let own_help = normalized(&String::from_utf8_lossy(&own.stdout));
        assert!(
            own_help.contains(&normalized(&own_help_claim(command))),
            "`{command} --help` does not print the description checked above: {own_help}"
        );
    }
}

/// Help and behaviour agree: what the description says, the invocation does.
///
/// The second probe carries an argument because the reported defect was found by
/// passing one — a shell name — and a pending command must answer the same way
/// however it is called rather than appearing to accept an operand.
#[test]
fn help_every_pending_command_behaves_as_its_help_describes() {
    for (command, reason) in PENDING_COMMANDS {
        for argv in [vec![*command], vec![*command, "anything"]] {
            let output = binary()
                .args(&argv)
                .output()
                .unwrap_or_else(|error| panic!("run {argv:?}: {error}"));
            assert!(
                !output.status.success(),
                "{argv:?} must not report success while its help says it cannot work"
            );
            assert!(
                output.stdout.is_empty(),
                "{argv:?} wrote {} bytes to stdout while refusing to work",
                output.stdout.len()
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(reason),
                "{argv:?} must explain itself with the recorded reason, got: {stderr}"
            );
            assert!(
                states_unavailability(&stderr),
                "{argv:?} must state that it cannot work, got: {stderr}"
            );
        }
        assert_eq!(
            pending_reason(command),
            Some(*reason),
            "`{command}`'s recorded reason must be reachable by name"
        );
    }
}

/// The negative control: the detector can see the line it was written to catch.
///
/// Without this, both checks above would keep passing if the predicates were
/// weakened to accept anything, and the roster's only entry happens to be worded
/// correctly today. The literal string is the one the binary shipped.
#[test]
fn help_failure_scenario_the_shipped_promise_is_rejected() {
    const SHIPPED_PROMISE: &str = "Generate shell completion output";

    assert!(
        !states_unavailability(SHIPPED_PROMISE),
        "the shipped line never said the command could not work; the marker check \
         must not accept it"
    );
    assert_eq!(
        opening_capability_promise(SHIPPED_PROMISE),
        Some("generate"),
        "the shipped line opened with a capability verb; the promise check must see it"
    );

    let current = root_listing_claim("completion");
    assert!(states_unavailability(&current), "{current}");
    assert_eq!(opening_capability_promise(&current), None, "{current}");
}

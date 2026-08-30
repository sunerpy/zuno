//! Help text may not advertise work a deliberately rejected command refuses.

use clap::CommandFactory as _;
use zuno_cli::{Cli, Disposition, disposition_for};

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

fn root_listing_claim(command: &str) -> String {
    Cli::command()
        .find_subcommand(command)
        .unwrap_or_else(|| panic!("`{command}` must be registered to have help"))
        .get_about()
        .unwrap_or_else(|| panic!("`{command}` must carry a description"))
        .to_string()
}

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

#[test]
fn help_completion_describes_the_capability_it_now_provides() {
    let claim = root_listing_claim("completion");
    assert_eq!(
        opening_capability_promise(&claim),
        Some("generate"),
        "{claim}"
    );
}

#[test]
fn help_failure_scenario_the_old_promise_is_detected() {
    const SHIPPED_PROMISE: &str = "Generate shell completion output";

    assert_eq!(
        opening_capability_promise(SHIPPED_PROMISE),
        Some("generate"),
        "the detector must recognize the promise that used to sit over a stub"
    );
}

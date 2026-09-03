//! Resource spellings: one file and one command reach the matcher under several
//! names, and a rule has to keep meaning the same thing for all of them.

use zuno_permission::{
    PermissionAction, Rule, canonical_path_resource, canonical_shell_resource, evaluate,
    rules_from_config,
};

fn rules_from_json(configuration: &str) -> Vec<Rule> {
    let config = serde_json::from_str(configuration).expect("fixture is valid permission config");
    rules_from_config(&config)
}

#[test]
fn the_canonical_path_spelling_drops_noise_but_keeps_parent_segments() {
    assert_eq!(canonical_path_resource("./src/main.rs"), "src/main.rs");
    assert_eq!(canonical_path_resource("src//main.rs"), "src/main.rs");
    assert_eq!(canonical_path_resource("src/./main.rs"), "src/main.rs");
    assert_eq!(canonical_path_resource("src/"), "src");
    assert_eq!(canonical_path_resource("."), ".");
    assert_eq!(canonical_path_resource("./"), ".");
    assert_eq!(
        canonical_path_resource("/ws//src/./main.rs"),
        "/ws/src/main.rs"
    );
    assert_eq!(canonical_path_resource("src\\main.rs"), "src/main.rs");
    assert_eq!(canonical_path_resource("C:\\ws\\src"), "C:/ws/src");
    assert_eq!(
        canonical_path_resource("/ws/../etc/passwd"),
        "/ws/../etc/passwd",
        "a lexical `..` can leave the directory a symlink pointed at, so the \
         canonical spelling keeps it and only a deny is widened past it"
    );
}

#[test]
fn the_canonical_shell_spelling_normalizes_the_program_and_the_spacing() {
    assert_eq!(canonical_shell_resource("rm  -rf  x"), "rm -rf x");
    assert_eq!(canonical_shell_resource("rm\t-rf x"), "rm -rf x");
    assert_eq!(canonical_shell_resource("'rm' -rf x"), "rm -rf x");
    assert_eq!(canonical_shell_resource("\"rm\" -rf x"), "rm -rf x");
    assert_eq!(canonical_shell_resource("\\rm -rf x"), "rm -rf x");
    assert_eq!(canonical_shell_resource("command rm -rf x"), "rm -rf x");
    assert_eq!(
        canonical_shell_resource("rm -rf 'a b'"),
        "rm -rf 'a b'",
        "an argument keeps the quoting the caller wrote"
    );
    assert_eq!(
        canonical_shell_resource("/bin/rm -rf x"),
        "/bin/rm -rf x",
        "the canonical spelling still names the executable that was invoked"
    );
}

#[test]
fn a_deny_written_absolute_also_covers_the_relative_spelling() {
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"read":{"*":"allow","/ws/.env":"deny"}}}"#);

    assert_eq!(evaluate("read", "/ws/.env", &rules), PermissionAction::Deny);
    assert_eq!(
        evaluate("read", ".env", &rules),
        PermissionAction::Deny,
        "the file tools name a workspace file relatively; the same rule must reach it"
    );
    assert_eq!(
        evaluate("read", "src/main.rs", &rules),
        PermissionAction::Allow
    );
}

#[test]
fn a_deny_written_relative_also_covers_the_absolute_spelling() {
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"read":{"*":"allow",".env":"deny"}}}"#);

    assert_eq!(evaluate("read", ".env", &rules), PermissionAction::Deny);
    assert_eq!(evaluate("read", "./.env", &rules), PermissionAction::Deny);
    assert_eq!(
        evaluate("read", "/ws/.env", &rules),
        PermissionAction::Deny,
        "the dispatch boundary names the file with the raw absolute argument"
    );
}

#[test]
fn a_deny_sees_through_a_parent_segment() {
    let rules = rules_from_json(
        r#"{"mode":"standard","rules":{"read":{"*":"allow","/etc/passwd":"deny"}}}"#,
    );

    assert_eq!(
        evaluate("read", "/ws/../etc/passwd", &rules),
        PermissionAction::Deny
    );
}

#[test]
fn an_absolute_deny_does_not_degrade_into_denying_every_relative_path() {
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"read":{"*":"allow","/tmp/*":"deny"}}}"#);

    assert_eq!(
        evaluate("read", "/tmp/scratch", &rules),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("read", "src/main.rs", &rules),
        PermissionAction::Allow,
        "shortening the pattern must stop before the all-wildcard tail"
    );
    assert_eq!(evaluate("read", ".env", &rules), PermissionAction::Allow);
}

#[test]
fn an_allow_still_needs_the_spelling_the_user_wrote() {
    // Relating an absolute path to a relative one is a guess: this crate is not told
    // the workspace root. Guessing widens a deny, which over-refuses, but it must
    // never widen a grant. The remaining gap is closed by making callers agree on
    // the canonical spelling, not by loosening the grant.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"read":{"*":"ask","/ws/src/*":"allow"}}}"#);

    assert_eq!(
        evaluate("read", "/ws/src/main.rs", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("read", "src/main.rs", &rules),
        PermissionAction::Ask
    );

    let relative =
        rules_from_json(r#"{"mode":"standard","rules":{"read":{"*":"ask","src/*":"allow"}}}"#);
    assert_eq!(
        evaluate("read", "src/main.rs", &relative),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("read", "./src/main.rs", &relative),
        PermissionAction::Allow,
        "`./` is the same spelling, not a different resource"
    );
    assert_eq!(
        evaluate("read", "/ws/src/main.rs", &relative),
        PermissionAction::Ask
    );
}

#[test]
fn opaque_resources_are_matched_verbatim() {
    // A URL, a query or an agent name is not a path and not a command line, so
    // nothing about it is rewritten.
    let rules = rules_from_json(
        r#"{"mode":"standard","rules":{"webfetch":"ask","task":{"*":"ask","build":"deny"}}}"#,
    );

    assert_eq!(evaluate("task", "build", &rules), PermissionAction::Deny);
    assert_eq!(evaluate("task", "/x/build", &rules), PermissionAction::Ask);
    assert_eq!(
        evaluate("webfetch", "https://example.invalid//a/./b", &rules),
        PermissionAction::Ask
    );
}

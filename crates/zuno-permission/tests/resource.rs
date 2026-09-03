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
    // `\rm` and the `command` builtin read differently under cmd and PowerShell, so
    // they are pinned per host by
    // `the_host_shell_decides_which_escape_and_which_builtin_the_identity_reading_has`
    // in `src/resource.rs`; asserting the POSIX answer here would fail a native
    // Windows run.
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
fn the_canonical_shell_spelling_removes_quoting_anywhere_in_the_program() {
    // A shell removes quotes and escape characters per character, not only around a
    // whole word, and it does that before it looks up the program. Every spelling
    // below therefore invokes `rm`.
    // Only the quote spellings are here: every shell removes a quote, while `\` is an
    // escape under a POSIX shell and a path separator under cmd and PowerShell. The
    // `\` rows are pinned per host in `src/resource.rs`.
    for command in [
        "rm\"\" -rf x",
        "\"\"rm -rf x",
        "r\"m\" -rf x",
        "\"r\"m -rf x",
        "'r'm -rf x",
        "r'm' -rf x",
        "r''m -rf x",
        "'r'\"m\" -rf x",
    ] {
        assert_eq!(
            canonical_shell_resource(command),
            "rm -rf x",
            "`{command}` names the program `rm`"
        );
    }
}

#[test]
fn the_canonical_shell_spelling_stops_where_the_program_stops_being_one_word() {
    assert_eq!(
        canonical_shell_resource("'\\rm' -rf x"),
        "\\rm -rf x",
        "single quotes are literal, so the backslash belongs to the program name"
    );
    assert_eq!(
        canonical_shell_resource("\"r\\m\" -rf x"),
        "r\\m -rf x",
        "inside double quotes a backslash only escapes what could end the string"
    );
    assert_eq!(
        canonical_shell_resource("rm\" \"-rf x"),
        "rm\" \"-rf x",
        "the program there is the single word `rm -rf`; respelling it would invent a \
         command line nobody ran"
    );
    assert_eq!(
        canonical_shell_resource("\"unterminated -rf x"),
        "\"unterminated -rf x",
        "no shell runs an unterminated quote, so there is nothing to canonicalize"
    );
    assert_eq!(
        canonical_shell_resource("r^m -rf x"),
        "r^m -rf x",
        "no POSIX shell honours cmd's escape character, so removing it widens a deny \
         only"
    );
}

#[test]
fn the_canonical_shell_spelling_leaves_a_dialect_or_a_glob_to_the_deny_side() {
    // `r$'m'` is `rm` under bash and zsh and the program `r$m` under dash, and
    // `/bin/r?` is whatever the glob expands to. None of those is a spelling of one
    // known program, so the canonical (identity) spelling keeps the token as written
    // and only a deny reads further.
    for command in [
        "r$'m' -rf x",
        "$\"rm\" -rf x",
        "rm$'' -rf x",
        "$'\\x72\\x6d' -rf x",
        "/bin/r? -rf x",
        "$PROG -rf x",
    ] {
        assert_eq!(
            canonical_shell_resource(command),
            command,
            "`{command}` names no one program, so the identity spelling is the token"
        );
    }
}

#[test]
fn the_canonical_shell_spelling_leaves_a_program_word_with_a_space_to_the_deny_side() {
    // The matcher compares one flattened command line, so a program word that contains
    // whitespace can line up with the rule's own program/argument boundary: reducing
    // `"/bin/rm -rf" /` to `/bin/rm -rf /` lets allow `/bin/rm -rf *` govern a file
    // literally named `/bin/rm -rf`. Naming a path does not change that, so the
    // reduction stays deny-only whatever the word looks like.
    for command in [
        "\"/opt/my tool/rm\" -rf x",
        "\"/bin/rm -rf\" /",
        "'/bin/rm -rf' /",
        "\"./tool.sh evil\"",
        "\"git commit\" -m x",
    ] {
        assert_eq!(
            canonical_shell_resource(command),
            command,
            "`{command}` names a program whose word contains a space, so the identity \
             spelling is the token as written"
        );
    }
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

#[test]
fn an_lsp_deny_reaches_both_spellings_the_tool_sends() {
    // `zuno-lsp` names the requested file relative to the worktree containing the
    // session, and falls back to the resolved absolute path where the session
    // directory and the worktree do not nest. Both spellings are production, so a
    // deny an author writes the way the repository reads it has to cover both — the
    // author cannot know which layout the session will have.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"lsp":{"*":"allow","secrets.rs":"deny"}}}"#);

    assert_eq!(
        evaluate("lsp", "secrets.rs", &rules),
        PermissionAction::Deny,
        "the nested layout sends the worktree-relative spelling"
    );
    assert_eq!(
        evaluate("lsp", "/home/u/proj/secrets.rs", &rules),
        PermissionAction::Deny,
        "the non-nested fallback sends the resolved absolute path for the same file"
    );
    assert_eq!(
        evaluate("lsp", "./secrets.rs", &rules),
        PermissionAction::Deny,
        "`./` is the same spelling, not a different resource"
    );
    assert_eq!(
        evaluate("lsp", "src/main.rs", &rules),
        PermissionAction::Allow,
        "widening the deny must stop at the file it names"
    );
}

#[test]
fn an_lsp_allow_still_needs_the_spelling_the_author_wrote() {
    // The suffix widening is deny-only, and adding `lsp` to the path keys must not
    // move it. This crate is not told the worktree root, so relating an absolute
    // request to a relative grant would be a guess, and a guess that widens a grant
    // authorizes a file nobody named.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"lsp":{"*":"ask","src/*":"allow"}}}"#);

    assert_eq!(
        evaluate("lsp", "src/main.rs", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("lsp", "/ws/src/main.rs", &rules),
        PermissionAction::Ask,
        "an absolute request is not the relative grant's resource"
    );
    assert_eq!(
        evaluate("lsp", "/etc/src/main.rs", &rules),
        PermissionAction::Ask,
        "and least of all when the suffix matches somewhere else entirely"
    );
}

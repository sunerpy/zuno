use proptest::prelude::*;
use zuno_permission::{PermissionAction, Rule, evaluate, rules_from_config};

fn rule(permission: &str, pattern: &str, action: PermissionAction) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: pattern.to_owned(),
        action,
    }
}

#[test]
fn later_ask_overrides_earlier_allow_for_the_same_command() {
    let rules = [
        rule("shell", "git *", PermissionAction::Allow),
        rule("shell", "*", PermissionAction::Ask),
    ];

    let action = evaluate("shell", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
}

#[test]
fn later_allow_overrides_earlier_ask_for_the_same_command() {
    let rules = [
        rule("shell", "*", PermissionAction::Ask),
        rule("shell", "git *", PermissionAction::Allow),
    ];

    let action = evaluate("shell", "git push", &rules);

    assert_eq!(action, PermissionAction::Allow);
}

#[test]
fn no_matching_rule_defaults_to_ask() {
    let rules = [rule("edit", "*", PermissionAction::Allow)];

    let action = evaluate("shell", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
}

#[test]
fn permission_keys_and_patterns_both_support_wildcards() {
    let rules = [rule("sh?ll", "git *", PermissionAction::Allow)];

    let action = evaluate("shell", "git status", &rules);

    assert_eq!(action, PermissionAction::Allow);
}

#[test]
fn arbitrary_custom_permission_keys_are_evaluated() {
    let rules = [rule("deploy_production", "blue-*", PermissionAction::Deny)];

    let action = evaluate("deploy_production", "blue-api", &rules);

    assert_eq!(action, PermissionAction::Deny);
}

#[test]
fn config_conversion_preserves_outer_and_nested_source_order() {
    let config = serde_json::from_str(
        r#"{
            "mode": "standard",
            "rules": {
                "*": "deny",
                "shell": {
                    "git *": "allow",
                    "*": "ask"
                },
                "deploy_production": "allow"
            }
        }"#,
    )
    .expect("fixture is valid permission config");

    let rules = rules_from_config(&config);

    assert_eq!(
        rules,
        vec![
            rule("*", "*", PermissionAction::Deny),
            rule("shell", "git *", PermissionAction::Allow),
            rule("shell", "*", PermissionAction::Ask),
            rule("deploy_production", "*", PermissionAction::Allow),
        ]
    );
}

#[test]
fn outer_config_key_order_controls_overlapping_permission_keys() {
    let config = serde_json::from_str(r#"{"mode":"standard","rules":{"shell":"allow","*":"ask"}}"#)
        .expect("fixture is valid permission config");
    let rules = rules_from_config(&config);

    let action = evaluate("shell", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
}

// ---------------------------------------------------------------------------
// Documented configuration examples.
//
// The JSON below is the corrected form of the examples in
// `docs/guide/permissions.md` ("Per-tool rules") and `docs/guide/headless.md`
// ("Permission modes without a human"). Both documents used to be written for a
// first-match-wins evaluator, which this engine is not: `evaluate` takes the
// **last** matching rule, so the catch-all has to come first and the narrow rules
// that override it have to come last.
//
// Keep the JSON here identical to the JSON in those documents. If either example
// is ever reordered back to "narrow first, catch-all last", copying it into these
// tests makes them fail, which is the point: the guide and the engine must not
// drift apart again.
// ---------------------------------------------------------------------------

/// `docs/guide/permissions.md`, section "Per-tool rules".
const DOCUMENTED_INTERACTIVE_CONFIG: &str = r#"{
  "mode": "standard",
  "rules": {
    "read": "allow",
    "edit": "ask",
    "shell": {
      "*": "ask",
      "git *": "allow",
      "git push*": "deny",
      "rm -rf*": "deny"
    }
  }
}"#;

/// `docs/guide/headless.md`, section "Permission modes without a human".
const DOCUMENTED_HEADLESS_CONFIG: &str = r#"{
  "mode": "standard",
  "rules": {
    "read": "allow",
    "glob": "allow",
    "grep": "allow",
    "edit": "deny",
    "shell": {
      "*": "deny",
      "cargo test*": "allow",
      "git push*": "deny"
    }
  }
}"#;

fn rules_from_json(configuration: &str) -> Vec<Rule> {
    let config = serde_json::from_str(configuration).expect("documented example is valid config");
    rules_from_config(&config)
}

#[test]
fn the_documented_interactive_example_denies_what_it_says_it_denies() {
    let rules = rules_from_json(DOCUMENTED_INTERACTIVE_CONFIG);

    assert_eq!(
        evaluate("shell", "rm -rf /", &rules),
        PermissionAction::Deny,
        "the guide presents this example as the one that stops `rm -rf /`"
    );
    assert_eq!(
        evaluate("shell", "git push origin main", &rules),
        PermissionAction::Deny,
        "`git push*` is written after `git *` so that a push is denied"
    );
    assert_eq!(
        evaluate("shell", "git status", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "cargo build", &rules),
        PermissionAction::Ask
    );
    assert_eq!(
        evaluate("read", "src/main.rs", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("edit", "src/main.rs", &rules),
        PermissionAction::Ask
    );
}

#[test]
fn the_documented_headless_example_allows_the_command_it_exists_to_allow() {
    let rules = rules_from_json(DOCUMENTED_HEADLESS_CONFIG);

    assert_eq!(
        evaluate("shell", "cargo test", &rules),
        PermissionAction::Allow,
        "the headless example exists to let CI run the test suite unattended"
    );
    assert_eq!(
        evaluate("shell", "cargo test -p zuno-permission", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "git push origin main", &rules),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "curl example.invalid", &rules),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("edit", "src/main.rs", &rules),
        PermissionAction::Deny
    );
    assert_eq!(evaluate("grep", "TODO", &rules), PermissionAction::Allow);
}

#[test]
fn first_match_wins_ordering_of_the_same_examples_loses_both_guarantees() {
    // Exactly the documented rules with the catch-all moved last, which is how both
    // guides used to print them. This is not a second opinion about ordering; it is
    // the evidence that the order carries the security property, so a guide that
    // prints the reverse order is printing a configuration that does not work.
    let interactive = rules_from_json(
        r#"{
          "mode": "standard",
          "rules": {
            "shell": {
              "git push*": "deny",
              "git *": "allow",
              "rm -rf*": "deny",
              "*": "ask"
            }
          }
        }"#,
    );
    assert_eq!(
        evaluate("shell", "rm -rf /", &interactive),
        PermissionAction::Ask,
        "the trailing catch-all swallows every narrow rule before it"
    );
    assert_eq!(
        evaluate("shell", "git push origin main", &interactive),
        PermissionAction::Ask
    );

    let headless = rules_from_json(
        r#"{
          "mode": "standard",
          "rules": {
            "shell": {
              "git push*": "deny",
              "cargo test*": "allow",
              "*": "deny"
            }
          }
        }"#,
    );
    assert_eq!(
        evaluate("shell", "cargo test", &headless),
        PermissionAction::Deny,
        "a trailing catch-all deny blocks the command the example allows"
    );
}

#[test]
fn a_wildcard_deny_covers_a_command_that_contains_a_star() {
    // The matcher used to compare a literal `*` in the command against the `*` in the
    // pattern and consume it, so this exact configuration did not stop `rm *.txt`.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm *":"deny"}}}"#);

    assert_eq!(
        evaluate("shell", "rm *.txt", &rules),
        PermissionAction::Deny
    );
    assert_eq!(evaluate("shell", "rm *", &rules), PermissionAction::Deny);
    assert_eq!(
        evaluate("shell", "rm -rf x", &rules),
        PermissionAction::Deny
    );
    assert_eq!(evaluate("shell", "rmdir x", &rules), PermissionAction::Ask);
}

// ---------------------------------------------------------------------------
// Shell rules match the command, not one spelling of it.
// ---------------------------------------------------------------------------

fn deny_rm_rf() -> Vec<Rule> {
    rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm -rf*":"deny"}}}"#)
}

#[test]
fn a_shell_deny_survives_extra_whitespace() {
    assert_eq!(
        evaluate("shell", "rm  -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "rm\t-rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_a_quoted_program() {
    assert_eq!(
        evaluate("shell", "'rm' -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "\"rm\" -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_an_escaped_program() {
    assert_eq!(
        evaluate("shell", "\\rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_an_absolute_program_path() {
    assert_eq!(
        evaluate("shell", "/bin/rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "/usr/bin/rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "./rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_the_command_builtin() {
    assert_eq!(
        evaluate("shell", "command rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "command /bin/rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_quoting_and_escaping_inside_the_program_token() {
    // Reversing a surrounding quote pair and a leading `\` covered `'rm'` and `\rm`
    // and nothing else, so every line below reached the catch-all and ran the
    // command the user had explicitly denied.
    for command in [
        "r\\m -rf /tmp/build",
        "\\r\\m -rf /tmp/build",
        "rm\"\" -rf /tmp/build",
        "\"\"rm -rf /tmp/build",
        "r\"m\" -rf /tmp/build",
        "\"r\"m -rf /tmp/build",
        "'r'm -rf /tmp/build",
        "r'm' -rf /tmp/build",
        "r''m -rf /tmp/build",
        "'r'\"m\" -rf /tmp/build",
        "command r\\m -rf /tmp/build",
        "'\\rm' -rf /tmp/build",
        // cmd escapes with `^` and PowerShell with a backtick. A deny has to hold
        // whichever interpreter runs the line.
        "r^m -rf /tmp/build",
        "r`m -rf /tmp/build",
        // The program here is the single word `rm -rf`, which no host has. Refusing
        // it anyway is the reading that fails safe.
        "rm\" \"-rf /tmp/build",
    ] {
        assert_eq!(
            evaluate("shell", command, &deny_rm_rf()),
            PermissionAction::Deny,
            "`{command}` is the denied command respelled"
        );
    }
}

#[test]
fn a_shell_deny_survives_a_windows_style_program_path() {
    // `\` separates a Windows path, so the file name it ends with is reached the same
    // way `/bin/rm` is reduced to `rm`.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm.exe *":"deny"}}}"#);

    assert_eq!(
        evaluate("shell", r"C:\Windows\System32\rm.exe -rf C:\build", &rules),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", r".\rm.exe -rf C:\build", &rules),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_dollar_quoting_in_the_program_token() {
    // `$'...'` is bash/zsh ANSI-C quoting and `$"..."` is bash locale quoting. Both
    // are *quoting*, not expansion, so every line below runs `rm` under bash —
    // measured with a fake `rm` on `PATH` — while `$` was read as an ordinary
    // literal here and the explicit deny degraded to the catch-all `ask`.
    for command in [
        "r$'m' -rf /tmp/build",
        "r$\"m\" -rf /tmp/build",
        "rm$'' -rf /tmp/build",
        "$'rm' -rf /tmp/build",
        "$\"rm\" -rf /tmp/build",
        // `\x72\x6d` spells `rm` only once it is decoded. The program is reported as
        // unresolvable instead of guessed, and the deny fails closed on it.
        "$'\\x72\\x6d' -rf /tmp/build",
        // The `command` builtin can be spelled the same way.
        "com$'m'and rm -rf /tmp/build",
    ] {
        assert_eq!(
            evaluate("shell", command, &deny_rm_rf()),
            PermissionAction::Deny,
            "`{command}` is the denied command respelled"
        );
    }
}

#[test]
fn a_shell_deny_survives_a_glob_in_the_program_token() {
    // `bash` expands `/bin/r?` to `/bin/rm` and runs it, and the matcher reads a `?`
    // in a *resource* as a literal character, so a program written as a glob used to
    // match no rule at all. A program this crate cannot resolve now fails closed on
    // the deny side instead.
    for command in [
        "/bin/r? -rf /tmp/build",
        "/bin/r[m] -rf /tmp/build",
        "/bin/r* -rf /tmp/build",
    ] {
        assert_eq!(
            evaluate("shell", command, &deny_rm_rf()),
            PermissionAction::Deny,
            "`{command}` can expand to the denied command"
        );
    }
}

#[test]
fn a_shell_deny_survives_an_unquoted_expansion_that_word_splits() {
    // An *unquoted* expansion, substitution or glob is word-split, so its result
    // supplies the program *and* its arguments. Measured with a fake `rm` and a fake
    // `git` on `PATH`: bash and dash both print `FAKE-RM invoked with args: -rf
    // /tmp/build` for the first two rows and for both quoting spellings of the
    // program, `FAKE-GIT invoked with args: push --force` for the `git` row below,
    // and bash, dash and zsh all run the two substitution rows. Every one of them is
    // a single token here, so reporting only the *program* as unresolvable let the
    // deny retry compare the bare rule program (`rm`) against `rm -rf*` and match
    // nothing, and the explicit prohibition degraded to `ask`.
    //
    // The `rm'` row is the one spelling of the five that no shell runs -- bash says
    // "unexpected EOF while looking for matching `''", dash "Unterminated quoted
    // string", zsh "unmatched '", and pwsh exits 1 without running anything. A token
    // no dialect can read is still not statically resolvable, and answering `ask`
    // because of that would be the same silent non-match, so it fails closed too.
    for command in [
        "rm${IFS}-rf${IFS}/tmp/build",
        "rm$IFS-rf$IFS/tmp/build",
        "rm'${IFS}-rf${IFS}/tmp/build",
        "$(echo rm -rf /tmp/build)",
        "`echo rm -rf /tmp/build`",
        "rm''${IFS}-rf${IFS}/tmp/build",
        "r$'m'${IFS}-rf${IFS}/tmp/build",
    ] {
        assert_eq!(
            evaluate("shell", command, &deny_rm_rf()),
            PermissionAction::Deny,
            "`{command}` runs the denied command, and a line this crate cannot read \
             at all has to assume it does"
        );
    }
    // It is not `rm`-specific.
    let deny_push =
        rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","git push*":"deny"}}}"#);
    assert_eq!(
        evaluate("shell", "git${IFS}push${IFS}--force", &deny_push),
        PermissionAction::Deny
    );
    // The bound. A *quoted* run is one word however it expands, so it supplies the
    // program only, and where argument tokens follow the run they are still the words
    // the shell will pass: both keep the retry that makes the arguments fit the rule.
    for command in ["$PROG status", "\"$PROG\"", "$'\\x72\\x6d'"] {
        assert_eq!(
            evaluate("shell", command, &deny_rm_rf()),
            PermissionAction::Ask,
            "`{command}` cannot run `rm -rf ...`, so `rm -rf*` is not a deny-everything"
        );
    }
}

#[test]
fn authorizing_a_long_unresolvable_command_line_stays_linear() {
    // `merge_open_substitution` re-scanned the whole accumulated program token once
    // per argument, which is quadratic in text the model controls, on a synchronous
    // path inside async authorization: 1 KB took 1.1 ms here, 40 KB took 1.19 s and
    // 120 KB took 9.59 s, all of it blocking the runtime. The scan is now carried
    // across tokens. The ceiling is deliberately generous so a loaded box does not
    // fail it; the quadratic version misses it by two orders of magnitude.
    let unit = "rm -rf /tmp/build ";
    let command = format!("`echo {}", unit.repeat(120 * 1024 / unit.len() + 1));
    assert!(command.len() > 120 * 1024, "the fixture is 120 KB of text");

    let started = std::time::Instant::now();
    let action = evaluate("shell", &command, &deny_rm_rf());
    let elapsed = started.elapsed();

    assert_eq!(
        action,
        PermissionAction::Deny,
        "an unterminated substitution is one unresolvable token, so the deny fails \
         closed on it"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "authorizing {} bytes took {elapsed:?}",
        command.len()
    );
}

#[test]
fn a_program_this_crate_cannot_resolve_fails_closed_only_where_the_rule_fits() {
    // An expansion or a substitution can name anything, so a deny is retried with the
    // program the *rule* names. That deliberately over-refuses — `$PROG -rf x` is
    // refused by `rm -rf*` — but it must not degrade into denying every command.
    let rules = deny_rm_rf();

    for command in [
        "$PROG -rf /tmp/build",
        "`which rm` -rf /tmp/build",
        "$(which rm) -rf /tmp/build",
        "${RM} -rf /tmp/build",
    ] {
        assert_eq!(
            evaluate("shell", command, &rules),
            PermissionAction::Deny,
            "`{command}` could be the denied command, and a deny that cannot tell \
             has to assume it is"
        );
    }
    assert_eq!(
        evaluate("shell", "$PROG status", &rules),
        PermissionAction::Ask,
        "the arguments still have to fit the rule; `rm -rf*` is not a deny-everything"
    );
    assert_eq!(
        evaluate("shell", "git commit -m \"$message\"", &rules),
        PermissionAction::Ask,
        "an expansion in an *argument* says nothing about which program ran"
    );
}

#[test]
fn an_allow_is_never_widened_by_a_dialect_the_host_may_not_be() {
    // `$'...'` runs `rm` under bash and zsh; dash reads `r$'m'` as the program `r$m`
    // and `$"rm"` as `$rm` (measured). A grant may not guess which of those the
    // configured interpreter is, and it may not guess at a glob either.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm -rf x":"allow"}}}"#);

    for command in [
        "r$'m' -rf x",
        "$\"rm\" -rf x",
        "rm$'' -rf x",
        "$'\\x72\\x6d' -rf x",
        "/bin/r? -rf x",
    ] {
        assert_eq!(
            evaluate("shell", command, &rules),
            PermissionAction::Ask,
            "`{command}` names `rm` only under some interpreters, so it may widen a \
             deny and never a grant"
        );
    }
}

#[test]
fn an_allow_never_governs_a_program_word_that_re_partitions_the_rule() {
    // Quote removal inside the program token is identity-preserving only while the
    // word stays one token. A word that contains whitespace can line up with the
    // rule's own program/argument boundary, and the matcher compares one flattened
    // command line, so the reduction would hand the grant to a *different* file: the
    // one literally named `/bin/rm -rf`, `./tool.sh evil` or `/usr/bin/git commit`,
    // each of them plantable with `cp evil.sh "./tool.sh evil"` in a directory the
    // agent can already write. Being path-shaped is orthogonal to that, so no shape
    // of a space-containing word reaches an `allow`.
    for (resource, pattern) in [
        ("\"./tool.sh evil\"", "./tool.sh *"),
        ("\"./tool.sh evil\" arg", "./tool.sh *"),
        ("\"/bin/rm -rf\" /", "/bin/rm -rf *"),
        ("'/bin/rm -rf' /", "/bin/rm -rf *"),
        ("\"/usr/bin/git commit\" -m x", "/usr/bin/git commit -m *"),
        ("\"/opt/my tool/rm\" -rf x", "/opt/my tool/rm -rf *"),
        ("'/opt/my tool/rm' -rf x", "/opt/my tool/rm -rf *"),
        ("\"git evil\"", "git *"),
    ] {
        let allow = vec![
            rule("shell", "*", PermissionAction::Ask),
            rule("shell", pattern, PermissionAction::Allow),
        ];
        assert_eq!(
            evaluate("shell", resource, &allow),
            PermissionAction::Ask,
            "`{pattern}` must not grant `{resource}`, whose program word contains a \
             space"
        );
        let deny = vec![
            rule("shell", "*", PermissionAction::Ask),
            rule("shell", pattern, PermissionAction::Deny),
        ];
        assert_eq!(
            evaluate("shell", resource, &deny),
            PermissionAction::Deny,
            "the same respelling is still refused by a deny"
        );
    }
    // The bound: the ordinary spelling of each of those rules still grants.
    let rules = rules_from_json(
        r#"{"mode":"standard","rules":{"shell":{"*":"ask","./tool.sh *":"allow","/opt/my tool/rm -rf *":"allow"}}}"#,
    );
    assert_eq!(
        evaluate("shell", "./tool.sh evil", &rules),
        PermissionAction::Allow,
        "`./tool.sh` really is the program the rule names"
    );
    assert_eq!(
        evaluate("shell", "/opt/my tool/rm -rf x", &rules),
        PermissionAction::Allow,
        "a rule whose own program path contains a space still matches the unquoted \
         command line the caller wrote"
    );
}

#[test]
fn an_allow_does_not_cover_a_bare_program_name_that_contains_a_space() {
    // `"git commit"` is looked up on `PATH`, where an executable literally named
    // `git commit` could otherwise inherit the grant. Only a *path* is admitted.
    let rules = rules_from_json(
        r#"{"mode":"standard","rules":{"shell":{"*":"ask","git commit -m *":"allow"}}}"#,
    );

    assert_eq!(
        evaluate("shell", "git commit -m x", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "\"git commit\" -m x", &rules),
        PermissionAction::Ask
    );
    assert_eq!(
        evaluate(
            "shell",
            "\"git commit\" -m x",
            &rules_from_json(
                r#"{"mode":"standard","rules":{"shell":{"*":"ask","git commit -m *":"deny"}}}"#
            )
        ),
        PermissionAction::Deny,
        "the same respelling is still refused by a deny"
    );
}

#[test]
fn an_allow_rule_does_not_inherit_a_spelling_only_a_deny_may_see() {
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm -rf x":"allow"}}}"#);

    assert_eq!(
        evaluate("shell", "'r'm -rf x", &rules),
        PermissionAction::Allow,
        "quote removal names the same program whichever shell reads the line"
    );
    assert_eq!(
        evaluate("shell", "r^m -rf x", &rules),
        PermissionAction::Ask,
        "removing cmd's escape character guesses the interpreter, so it may only \
         widen a deny"
    );
    assert_eq!(
        evaluate("shell", "rm\" \"-rf x", &rules),
        PermissionAction::Ask,
        "a grant covers the program the user named, not a different word that \
         re-tokenizes into it"
    );
}

#[test]
fn the_raw_spelling_keeps_matching_the_rules_written_for_it() {
    // Normalization only ever adds spellings. A rule written against the exact text
    // a user sees in their terminal must keep working.
    let rules = rules_from_json(
        r#"{"mode":"standard","rules":{"shell":{"*":"ask","/bin/rm *":"deny","'quoted' *":"deny"}}}"#,
    );

    assert_eq!(
        evaluate("shell", "/bin/rm -rf /tmp/build", &rules),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "'quoted' argument", &rules),
        PermissionAction::Deny
    );
}

#[test]
fn an_allow_rule_never_inherits_the_identity_of_another_program() {
    // The relaxations that drop which executable was named widen a deny only. An
    // allow for `rm` must not authorize a different `rm` found somewhere else.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm *":"allow"}}}"#);

    assert_eq!(
        evaluate("shell", "rm  /tmp/build", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "'rm' /tmp/build", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "/tmp/attacker/rm /tmp/build", &rules),
        PermissionAction::Ask,
        "an allow rule must not cover an unrelated executable that shares a file name"
    );
}

// ---------------------------------------------------------------------------
// Home expansion.
// ---------------------------------------------------------------------------

#[test]
fn home_expansion_requires_a_path_boundary() {
    let home = dirs::home_dir().expect("test host has a home directory");
    let home = home.to_string_lossy().into_owned();
    let rules = rules_from_json(
        r#"{"mode":"standard","rules":{"read":{"$HOME/x":"deny","$HOMEBREW/*":"deny","$HOME":"deny","~/y":"deny"}}}"#,
    );

    let patterns: Vec<&str> = rules.iter().map(|rule| rule.pattern.as_str()).collect();
    assert_eq!(
        patterns,
        vec![
            format!("{home}/x").as_str(),
            "$HOMEBREW/*",
            home.as_str(),
            format!("{home}/y").as_str(),
        ],
        "`$HOME` expands only when the next character starts a new path segment"
    );
}

fn action_strategy() -> impl Strategy<Value = PermissionAction> {
    prop_oneof![
        Just(PermissionAction::Ask),
        Just(PermissionAction::Allow),
        Just(PermissionAction::Deny),
    ]
}

proptest! {
    #[test]
    fn random_rule_orders_use_the_last_match_and_empty_rules_ask(
        specs in prop::collection::vec((any::<bool>(), action_strategy()), 0..64)
    ) {
        let rules: Vec<_> = specs
            .iter()
            .enumerate()
            .map(|(index, (matches, action))| {
                if *matches {
                    let permission = if index % 2 == 0 { "shell" } else { "s*" };
                    let pattern = if index % 3 == 0 { "git push" } else { "git *" };
                    rule(permission, pattern, *action)
                } else {
                    rule("edit", "cargo *", *action)
                }
            })
            .collect();
        let expected = specs
            .iter()
            .rev()
            .find(|(matches, _)| *matches)
            .map_or(PermissionAction::Ask, |(_, action)| *action);

        prop_assert_eq!(evaluate("shell", "git push", &rules), expected);
        prop_assert_eq!(evaluate("shell", "git push", &[]), PermissionAction::Ask);
    }
}

// ---------------------------------------------------------------------------
// Path rules: an allow matches respellings of the same path, a deny reaches
// further. `docs/guide/permissions.md`, section "Per-tool rules", tells users to
// plan around this asymmetry, so it is pinned here.
// ---------------------------------------------------------------------------

fn path_rules(action: PermissionAction) -> Vec<Rule> {
    vec![
        rule("edit", "*", PermissionAction::Ask),
        rule("edit", "src/main.rs", action),
    ]
}

#[test]
fn an_allow_covers_every_spelling_of_the_path_it_names() {
    let rules = path_rules(PermissionAction::Allow);

    for resource in [
        "src/main.rs",
        "./src/main.rs",
        "src//main.rs",
        "src\\main.rs",
        "./src/./main.rs",
    ] {
        assert_eq!(
            evaluate("edit", resource, &rules),
            PermissionAction::Allow,
            "{resource} is the same file the rule names, spelled differently"
        );
    }
}

#[test]
fn an_allow_never_widens_to_a_path_the_rule_did_not_name() {
    let rules = path_rules(PermissionAction::Allow);

    // The documented `filePath` contract is an absolute path, so a relative allow
    // does not cover the absolute one. Widening it would authorize a file outside
    // the workspace that happens to share the tail, which is why the guide tells
    // users to write an allow with `~`, `$HOME`, an absolute prefix, or a wildcard.
    assert_eq!(
        evaluate("edit", "/ws/src/main.rs", &rules),
        PermissionAction::Ask
    );
    assert_eq!(
        evaluate("edit", "other/src/main.rs", &rules),
        PermissionAction::Ask
    );
    assert_eq!(
        evaluate(
            "edit",
            "/ws/src/main.rs",
            &rules_from_json(
                r#"{"mode":"standard","rules":{"edit":{"*":"ask","*/src/*":"allow"}}}"#
            )
        ),
        PermissionAction::Allow,
        "the wildcard spelling the guide recommends does reach the absolute path"
    );
}

#[test]
fn a_deny_reaches_the_absolute_and_parent_resolved_spellings_too() {
    let rules = path_rules(PermissionAction::Deny);

    for resource in [
        "src/main.rs",
        "./src/main.rs",
        "/ws/src/main.rs",
        "src/../src/main.rs",
    ] {
        assert_eq!(
            evaluate("edit", resource, &rules),
            PermissionAction::Deny,
            "a deny must not be sidestepped by respelling {resource}"
        );
    }
}

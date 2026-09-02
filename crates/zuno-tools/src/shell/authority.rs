//! What a command's own text does to the exit contract its interpreter agreed to.
//!
//! # Why the wrapper is not the whole story
//!
//! [`super::exit_contract`] reads the configuration a command runs under: the
//! interpreter, and the `set` prologue that interpreter could honour. That is a
//! fact about the *shell*, and it is the right starting point — but the caller's
//! text runs inside that shell and can take the guarantee back apart. `cargo test
//! || true` exits zero whatever the tests did. `set +e` turns off the option the
//! prologue set. `bash -c 'cargo build | tee log'` runs a pipeline in a shell that
//! never saw our `pipefail`. Under `pipefail`, which sets no `-e`, a two-statement
//! script reports only the second statement's status.
//!
//! `set -e` is narrower than it reads, too. POSIX exempts every member of an `&&` or
//! `||` list but the last, and the exempt member is exactly the one that failed when
//! the list short-circuits, so `cargo build && cargo test` followed by anything else
//! exits on that other statement's status. An `if` or `case` whose condition fails or
//! whose patterns miss exits zero under every policy, and a loop reports its last
//! iteration. None of those are pipelines, so `pipefail` does not see them either.
//!
//! None of those are bugs in the wrapper. [`super::posix_script`] deliberately
//! keeps the caller's command at the same shell level so their `set`, `trap`, `cd`
//! and `exec` behave exactly as they would without us, and a command that turns an
//! option back off is *meant* to win. What must not happen is the receipt claiming
//! [`ExitAuthority::Authoritative`] afterwards, because a criterion closed on that
//! receipt would be closed on a status that could not have failed.
//!
//! So this module changes no execution behaviour at all. It reads the command that
//! is about to run and, when the text defeats the configuration, downgrades the
//! *claim* to [`ExitAuthority::Derived`] with a limitation that names the
//! construct and the remedy.
//!
//! # Why the syntax tree and not a substring search
//!
//! `echo "run tests || true"` contains `||` and masks nothing. A grep-shaped check
//! would flag it, and flagging honest commands is how a gate gets switched off.
//! The tool already parses every command with tree-sitter to decide permission
//! resources and destructive-command risk, so the same tree answers this question
//! without a second, disagreeing notion of what the text says: a `||` inside a
//! string literal is a string, and a heredoc body is one token rather than a list
//! of statements.
//!
//! # What it does not claim to catch
//!
//! A test that always passes, a `make` recipe whose own `sh` masks a pipeline
//! stage, a script file invoked by path — the status is honest about the process
//! that ran, and no static reading of one command can go further. The limitation
//! text is therefore about *this* command's structure, never a promise that a
//! clean report means the work is correct.

use tree_sitter::{Node, Parser};

use super::{CommandShellKind, ExitPolicy};

/// Container nodes that hold a list of statements rather than being one.
///
/// Named for both grammars: a `program` is bash's root and PowerShell's, and the
/// remaining kinds are the places either grammar puts a sequence of statements
/// that the caller wrote on one line or in one block.
const STATEMENT_CONTAINERS: &[&str] = &[
    "program",
    "compound_statement",
    "subshell",
    "statement_list",
    "script_block",
    "script_block_body",
    "statement_block",
];

/// Nodes that are not statements even though the grammar names them.
///
/// A heredoc body is listed because tree-sitter-bash has attached it both as a
/// child of the redirect and as a sibling of the command that opened it; counting
/// it would make `git commit -F - <<'MSG'` read as two statements.
const NOT_A_STATEMENT: &[&str] = &["comment", "heredoc_body", "heredoc_start", "heredoc_end"];

/// Commands whose argument list is a script this wrapper's prologue never reaches.
const NESTED_INTERPRETERS: &[&str] = &[
    "ash",
    "bash",
    "busybox",
    "csh",
    "dash",
    "fish",
    "ksh",
    "mksh",
    "powershell",
    "pwsh",
    "sh",
    "tcsh",
    "zsh",
];

/// Words that stand in front of the command they run without changing its status.
///
/// Kept separate from [`crate::risk`]'s wrapper table because the question here is
/// narrower: not "what will this touch" but "which word names the program whose
/// exit status the shell will see".
const TRANSPARENT_PREFIXES: &[&str] = &[
    "command", "env", "exec", "nice", "nohup", "stdbuf", "sudo", "time", "timeout",
];

/// The limitation `command`'s own text puts on an otherwise authoritative status.
///
/// Returns `None` when the text leaves the configuration's guarantee intact, which
/// is the common case: one program, or an `&&` chain, whose status the shell
/// reports as its own.
///
/// Only called for a configuration [`super::exit_contract`] would otherwise call
/// authoritative, so the prologue can be assumed to be the full one for `policy`.
/// A command this module cannot parse yields `None`: the configuration's verdict
/// stands rather than being overridden by a failed reading of the text.
pub(super) fn text_limitation(
    kind: CommandShellKind,
    policy: ExitPolicy,
    command: &str,
) -> Option<String> {
    let mut parser = Parser::new();
    let language = match kind {
        CommandShellKind::Posix => tree_sitter_bash::LANGUAGE.into(),
        CommandShellKind::PowerShell => tree_sitter_powershell::LANGUAGE.into(),
    };
    parser.set_language(&language).ok()?;
    let tree = parser.parse(command, None)?;
    let root = tree.root_node();
    // A tree the grammar could not fit is not a reading of the command. The
    // configuration's verdict stands, the command still runs, and the shell decides
    // what the text meant - which is the same order of authority as everywhere else
    // here.
    if root.has_error() {
        return None;
    }
    let source = command.as_bytes();
    match kind {
        CommandShellKind::Posix => posix_limitation(root, source, policy),
        CommandShellKind::PowerShell => powershell_limitation(root, source),
    }
}

/// The first construct in a POSIX command that outlives the prologue.
///
/// Ordered by how deliberately the construct removes the guarantee, so a command
/// doing several of these reports the one a reader most needs to see.
fn posix_limitation(root: Node<'_>, source: &[u8], policy: ExitPolicy) -> Option<String> {
    if let Some(token) = find_option_reversal(root, source) {
        return Some(format!(
            "the command runs `{token}`, which turns off the option this tool's prologue set, so a \
             failure after it need not be reflected in this exit status"
        ));
    }
    if let Some(fallback) = find_masking_fallback(root, source) {
        return Some(format!(
            "the command guards work with `|| {fallback}`, so a zero exit status can mean the \
             fallback ran rather than that the work succeeded; drop the fallback to have the \
             status decide, or read the output"
        ));
    }
    if let Some(interpreter) = find_nested_interpreter(root, source) {
        return Some(format!(
            "the command hands a script to `{interpreter}`, and that shell runs under its own \
             options rather than the ones set here, so a failure inside the script need not be \
             reflected in this exit status"
        ));
    }
    if let Some(limitation) = find_masked_compound(root, source, policy) {
        return Some(limitation);
    }
    // `all` prepends `set -e`, which stops a plain list at the first failing statement.
    // It does not stop for a failure inside an `&&`/`||` list, and when such a list is
    // not the last statement its status is discarded, so the failure leaves nothing
    // behind. Under `pipefail` the rule below already covers the same text.
    if matches!(policy, ExitPolicy::All)
        && let Some(list) = find_nonfinal_and_or_list(root, source)
    {
        return Some(format!(
            "the command runs `{list}` before its last statement, and `set -e` does not stop for \
             a failure inside an `&&` or `||` list, so that failure is not reflected in this exit \
             status; make the guarded work the last statement, or run one statement per call"
        ));
    }
    if matches!(policy, ExitPolicy::Pipefail) && statement_count(root) > 1 {
        return Some(format!(
            "the command runs more than one statement and exitPolicy \"{}\" reports only the last \
             one, so an earlier statement's failure is not reflected in this exit status; join the \
             statements with `&&`, or use exitPolicy \"{}\" to stop at the first failure",
            ExitPolicy::Pipefail.as_str(),
            ExitPolicy::All.as_str()
        ));
    }
    None
}

/// The limitation a multi-statement PowerShell command carries under `all`.
///
/// `all` is the only policy PowerShell reaches this function with, and its script
/// re-raises `$LASTEXITCODE` once, after the whole command. `$ErrorActionPreference
/// = 'Stop'` does not promote a *native* command's failure to a terminating error
/// on Windows PowerShell or on pwsh before 7.4, so an earlier native command that
/// exited non-zero leaves nothing behind for that final check to find. One
/// statement has nothing earlier to lose, which is why the count is the test.
fn powershell_limitation(root: Node<'_>, source: &[u8]) -> Option<String> {
    if statement_count(root) > 1 {
        return Some(
            "the command runs more than one statement and only the last one's `$LASTEXITCODE` is \
             re-raised, so an earlier native command's failure is not reflected in this exit \
             status; run one statement per call to have the status decide"
                .to_owned(),
        );
    }
    find_uncovered_pipeline(root, source)
}

/// The first `|` pipeline whose earlier stages `$LASTEXITCODE` cannot carry.
///
/// One statement has no earlier *statement* to lose, but a pipeline is one statement
/// with several commands in it, and `$LASTEXITCODE` holds the last native command's
/// code alone: `cargo test 2>&1 | findstr FAIL` reports what `findstr` did about the
/// output, never what `cargo` did about the tests.
///
/// A cmdlet is a different matter. `$ErrorActionPreference = 'Stop'` promotes its
/// errors to terminating ones, which end the script whatever position it sits in, so
/// `Get-Content log | Select-String fail` is covered. Only a hyphenated name is
/// reliably a cmdlet rather than a native command or an alias, so the pipeline is
/// reported when two or more stages could be native and left alone when at most one
/// could be — the last of which is the stage the re-raise does see.
fn find_uncovered_pipeline(root: Node<'_>, source: &[u8]) -> Option<String> {
    first_match(root, &mut |node| {
        if !pipes_into_a_stage(node, source) {
            return None;
        }
        let stages = pipeline_stage_programs(node, source);
        let native = stages.iter().filter(|name| !name.contains('-')).count();
        (native > 1).then(|| {
            "the command pipes one native command into another and `$LASTEXITCODE` holds only the \
             last one's code, so an earlier stage's failure is not reflected in this exit status; \
             run the check without a pipeline, or read the output"
                .to_owned()
        })
    })
}

/// Whether `node` joins stages with a literal `|`.
///
/// Read off the operator token rather than the node's kind, because the two grammars
/// disagree about which named node owns a pipe and a wrong kind name would silently
/// find nothing. A `|` inside a string is part of that string's node and has no token
/// of its own, which is the whole reason this reads the tree instead of the text.
fn pipes_into_a_stage(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| !child.is_named() && child.utf8_text(source).is_ok_and(|text| text == "|"))
}

/// The program each stage of `pipeline` names, lowercased, in source order.
fn pipeline_stage_programs(pipeline: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut cursor = pipeline.walk();
    pipeline
        .named_children(&mut cursor)
        .filter_map(first_command)
        .filter_map(|command| {
            command_words(command, source)
                .first()
                .map(|word| program_name(word).to_ascii_lowercase())
        })
        .collect()
}

/// The first `command` node at or under `node`.
fn first_command<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    if node.kind() == "command" {
        return Some(node);
    }
    let mut cursor = node.walk();
    let children = node.children(&mut cursor).collect::<Vec<_>>();
    children.into_iter().find_map(first_command)
}

/// The first compound statement whose own status cannot report a failure inside it.
///
/// The condition of an `if`, a `while` or an `until` is exempt from `set -e`, and its
/// failure is how the construct normally ends, so a check written there reports zero
/// either way; a `case` that matches no pattern exits zero for the same reason. A
/// `for` loop reports its last iteration alone, which under `pipefail` hides every
/// earlier one, while under `all` `set -e` stops in the body at the first failure —
/// unless the body guards that work with `&&` or `||`, which `-e` exempts.
///
/// A construct that re-raises — `if ! cargo test; then exit 1; fi` — is left alone.
/// Its author has already made the failure the command's status, and telling them
/// otherwise is how a reader learns to skip the limitation text.
fn find_masked_compound(root: Node<'_>, source: &[u8], policy: ExitPolicy) -> Option<String> {
    outcome_statements(root).into_iter().find_map(|statement| {
        let reason = match statement.kind() {
            "if_statement" => {
                "the command's outcome is an `if`, which exits zero when its condition fails and \
                 when no branch runs, so a check written as that condition reports success either \
                 way"
            }
            "while_statement" | "until_statement" => {
                "the command's outcome is a loop, which exits zero when its condition fails, so a \
                 check written as that condition reports success either way"
            }
            "case_statement" => {
                "the command's outcome is a `case`, which exits zero when no pattern matches, so \
                 nothing inside it need have run"
            }
            "for_statement" | "c_style_for_statement" => match policy {
                ExitPolicy::Pipefail => {
                    "the command's outcome is a loop, which reports only its last iteration, so an \
                     earlier iteration's failure is not reflected in this exit status"
                }
                ExitPolicy::All if contains_and_or_list(statement) => {
                    "the command's outcome is a loop whose body guards work with `&&` or `||`, \
                     which `set -e` does not stop for, so an earlier iteration's failure is not \
                     reflected in this exit status"
                }
                // `set -e` stops in the body at the first failing statement, so the loop's
                // own status is the honest one here. `last` never reaches this module: that
                // configuration is derived before any text is read.
                ExitPolicy::All | ExitPolicy::Last => return None,
            },
            _ => return None,
        };
        if re_raises_a_failure(statement, source) {
            return None;
        }
        Some(format!(
            "{reason}; run the check as its own call to have the status decide, or read the output"
        ))
    })
}

/// Whether the caller wrote an explicit re-raise anywhere inside `node`.
///
/// `exit`, `return` with a failing code, and `false` are how a compound hands its own
/// failure back to the shell. Asked of the whole subtree rather than of the branch
/// that will run, because which branch runs is not knowable here and one re-raise is
/// enough to show the author is not resting on the construct's own status.
fn re_raises_a_failure(node: Node<'_>, source: &[u8]) -> bool {
    first_match(node, &mut |node| {
        (node.kind() == "command" && propagates_failure(&command_words(node, source)))
            .then(|| "re-raised".to_owned())
    })
    .is_some()
}

/// The statements whose status the shell could report as this command's own.
///
/// The outermost list the caller wrote, with `&&`/`||` chains opened up, because a
/// compound sitting in one of those decides the command's outcome just as much as one
/// written on its own line. A compound nested deeper — inside a function body, a
/// subshell or another compound — is reached through the statement that contains it,
/// and that statement is the one whose status the caller will read.
fn outcome_statements<'tree>(root: Node<'tree>) -> Vec<Node<'tree>> {
    let mut statements = Vec::new();
    for statement in outermost_statements(root) {
        push_outcome(statement, &mut statements);
    }
    statements
}

/// Adds `statement`, or the members of the `&&`/`||` chain it is, to `statements`.
fn push_outcome<'tree>(statement: Node<'tree>, statements: &mut Vec<Node<'tree>>) {
    if statement.kind() == "list" {
        for member in named_statements(statement) {
            push_outcome(member, statements);
        }
        return;
    }
    statements.push(statement);
}

/// Whether an `&&` or `||` list appears anywhere under `node`.
fn contains_and_or_list(node: Node<'_>) -> bool {
    if node.kind() == "list" {
        return true;
    }
    let mut cursor = node.walk();
    let children = node.children(&mut cursor).collect::<Vec<_>>();
    children.into_iter().any(contains_and_or_list)
}

/// The first `&&`/`||` list the caller wrote before their last statement, as written.
fn find_nonfinal_and_or_list(root: Node<'_>, source: &[u8]) -> Option<String> {
    let statements = outermost_statements(root);
    let (_, earlier) = statements.split_last()?;
    earlier.iter().find_map(|statement| {
        if statement.kind() != "list" {
            return None;
        }
        Some(summarize(statement.utf8_text(source).ok()?.trim()))
    })
}

/// How many statements the caller wrote in the outermost list they wrote it in.
///
/// Descends through containers that hold exactly one statement, so a grammar that
/// wraps the root in a list of one does not read as one statement when that
/// statement is itself a list of several. Comments do not count.
fn statement_count(node: Node<'_>) -> usize {
    outermost_statements(node).len()
}

/// The statements of the outermost list the caller wrote, in source order.
fn outermost_statements<'tree>(node: Node<'tree>) -> Vec<Node<'tree>> {
    let mut current = node;
    loop {
        let statements = named_statements(current);
        match statements.len() {
            1 if STATEMENT_CONTAINERS.contains(&statements[0].kind()) => current = statements[0],
            _ => return statements,
        }
    }
}

/// The named children of `node` that are statements in their own right.
fn named_statements<'tree>(node: Node<'tree>) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !NOT_A_STATEMENT.contains(&child.kind()))
        .collect()
}

/// The first `set +…` the command runs, as written.
fn find_option_reversal(root: Node<'_>, source: &[u8]) -> Option<String> {
    first_match(root, &mut |node| {
        if node.kind() != "command" {
            return None;
        }
        let words = command_words(node, source);
        let (program, arguments) = words.split_first()?;
        if program != "set" || !arguments.iter().any(|word| word.starts_with('+')) {
            return None;
        }
        Some(words.join(" "))
    })
}

/// The right-hand side of the first `||` that can turn a failure into success.
///
/// A `||` whose fallback exits non-zero — `cargo test || exit 1` — preserves the
/// failure it catches, so it is not a masking fallback and not reported. Anything
/// else is: the shell reports the fallback's status, and a fallback that succeeds
/// is indistinguishable from work that succeeded.
fn find_masking_fallback(root: Node<'_>, source: &[u8]) -> Option<String> {
    first_match(root, &mut |node| {
        if node.kind() != "list" {
            return None;
        }
        let mut cursor = node.walk();
        if !node.children(&mut cursor).any(|child| {
            !child.is_named() && child.utf8_text(source).is_ok_and(|text| text == "||")
        }) {
            return None;
        }
        let fallback = {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).last()?
        };
        let words = command_words(fallback, source);
        if propagates_failure(&words) {
            return None;
        }
        let text = fallback.utf8_text(source).ok()?.trim();
        Some(summarize(text))
    })
}

/// Whether a `||` fallback leaves the shell with a failing status.
fn propagates_failure(words: &[String]) -> bool {
    let Some((program, arguments)) = words.split_first() else {
        return false;
    };
    match program.as_str() {
        // `|| exit` with no code re-exits with the status of the command that failed.
        "exit" | "return" => arguments
            .first()
            .is_none_or(|code| code.parse::<i64>().is_ok_and(|code| code != 0)),
        "false" => true,
        _ => false,
    }
}

/// The first interpreter the command hands a script to, as the caller named it.
fn find_nested_interpreter(root: Node<'_>, source: &[u8]) -> Option<String> {
    first_match(root, &mut |node| {
        if node.kind() != "command" {
            return None;
        }
        let words = command_words(node, source);
        let mut words = words.iter().map(String::as_str).skip_while(|word| {
            TRANSPARENT_PREFIXES.contains(&program_name(word)) || word.contains('=')
        });
        let program = words.next()?;
        let name = program_name(program);
        if !NESTED_INTERPRETERS.contains(&name) {
            return None;
        }
        let carries_script = words.clone().any(is_script_option) || feeds_a_heredoc(node);
        carries_script.then(|| program.to_owned())
    })
}

/// Whether this command's standard input is a heredoc the caller wrote inline.
///
/// A heredoc into an interpreter is a script by another route, and it is the shape
/// a multi-line inline script usually takes.
fn feeds_a_heredoc(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "redirected_statement" {
        return false;
    }
    let mut cursor = parent.walk();
    parent
        .named_children(&mut cursor)
        .any(|child| child.kind().starts_with("heredoc"))
}

/// Whether a word is the option that introduces an inline script.
fn is_script_option(word: &str) -> bool {
    matches!(word, "-c" | "-Command" | "-command" | "--command") || word.starts_with("-c")
}

/// The program a word names, with any directory and any quoting removed.
fn program_name(word: &str) -> &str {
    let word = word
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\'']);
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

/// The words of one `command` node, quoting left as the caller wrote it.
fn command_words(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut words = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "command_elements" {
            let mut elements = child.walk();
            for item in child.children(&mut elements) {
                if let Ok(text) = item.utf8_text(source) {
                    let text = text.trim();
                    if !text.is_empty() {
                        words.push(text.to_owned());
                    }
                }
            }
            continue;
        }
        if matches!(
            child.kind(),
            "command_name"
                | "command_name_expr"
                | "word"
                | "number"
                | "string"
                | "raw_string"
                | "concatenation"
                | "variable_assignment"
        ) && let Ok(text) = child.utf8_text(source)
        {
            let text = text.trim();
            if !text.is_empty() {
                words.push(text.to_owned());
            }
        }
    }
    words
}

/// The first node anywhere in the tree that `probe` accepts, in source order.
fn first_match(
    node: Node<'_>,
    probe: &mut dyn FnMut(Node<'_>) -> Option<String>,
) -> Option<String> {
    if let Some(found) = probe(node) {
        return Some(found);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = first_match(child, probe) {
            return Some(found);
        }
    }
    None
}

/// One line of a construct, short enough to sit inside a limitation sentence.
fn summarize(text: &str) -> String {
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 40;
    if folded.len() <= MAX {
        return folded;
    }
    let mut end = MAX - '…'.len_utf8();
    while end > 0 && !folded.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", folded[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limitation `command` earns as a POSIX command under `policy`.
    fn posix(policy: ExitPolicy, command: &str) -> Option<String> {
        text_limitation(CommandShellKind::Posix, policy, command)
    }

    #[test]
    fn one_command_or_an_and_chain_keeps_the_configuration_s_verdict() {
        for command in [
            "cargo test --workspace",
            "cd crates/zuno-goal && cargo test",
            "cargo test | tail -5",
            "cargo test 2>&1 | tail -5",
            "cargo test --workspace \\\n  --all-targets",
            "git commit -F - <<'MSG'\nfeat: land it\nMSG",
        ] {
            for policy in [ExitPolicy::Pipefail, ExitPolicy::All] {
                assert_eq!(posix(policy, command), None, "{policy:?}: {command}");
            }
        }
    }

    #[test]
    fn a_fallback_that_cannot_fail_is_reported_rather_than_trusted() {
        let limitation = posix(ExitPolicy::All, "cargo test || true").expect("masked status");
        assert!(limitation.contains("|| true"), "{limitation}");
        assert!(limitation.contains("fallback ran"), "{limitation}");
        for command in [
            "cargo test || :",
            "cargo test || echo 'tests failed'",
            "make check || warn",
        ] {
            assert!(posix(ExitPolicy::Pipefail, command).is_some(), "{command}");
        }
    }

    #[test]
    fn a_fallback_that_re_raises_the_failure_is_left_alone() {
        for command in [
            "cargo test || exit 1",
            "cargo test || exit",
            "cargo test || false",
        ] {
            assert_eq!(posix(ExitPolicy::All, command), None, "{command}");
        }
        // `|| exit 0` is the same masking as `|| true`, spelled differently.
        assert!(posix(ExitPolicy::All, "cargo test || exit 0").is_some());
    }

    #[test]
    fn turning_the_option_back_off_is_named_in_the_limitation() {
        for command in [
            "set +e\ncargo test",
            "set +o pipefail\ncargo test | tail -5",
            "set +eo pipefail; cargo test",
        ] {
            let limitation = posix(ExitPolicy::All, command).expect("reversed option");
            assert!(
                limitation.starts_with("the command runs `set +"),
                "{limitation}"
            );
            assert!(limitation.contains("prologue set"), "{limitation}");
        }
        // Only `+` takes a guarantee away. A caller adding `set -e` strengthens it.
        assert_eq!(posix(ExitPolicy::All, "set -e\ncargo test"), None);
    }

    #[test]
    fn a_script_handed_to_another_shell_is_reported_as_out_of_reach() {
        for command in [
            "bash -c 'cargo build | tee build.log'",
            "sh -c 'cargo test'",
            "env FOO=1 bash -c 'cargo test'",
            "exec bash -c 'cargo test'",
            "/bin/sh -c 'cargo test'",
            "sh <<'EOF'\ncargo build\ncargo test\nEOF",
        ] {
            let limitation = posix(ExitPolicy::All, command)
                .unwrap_or_else(|| panic!("inner shell not reported: {command}"));
            assert!(limitation.contains("its own options"), "{limitation}");
        }
    }

    #[test]
    fn an_interpreter_named_only_inside_a_string_is_not_an_inner_shell() {
        for command in [
            "echo 'run bash -c cargo test'",
            "grep -rn 'sh -c' crates",
            "python3 - <<'PY'\nprint('ok')\nPY",
        ] {
            assert_eq!(posix(ExitPolicy::All, command), None, "{command}");
        }
    }

    #[test]
    fn a_statement_list_is_only_covered_by_the_policy_that_stops_at_a_failure() {
        let list = "cargo build; cargo test";
        let limitation = posix(ExitPolicy::Pipefail, list).expect("only the last statement");
        assert!(
            limitation.contains("more than one statement"),
            "{limitation}"
        );
        assert!(limitation.contains("exitPolicy \"all\""), "{limitation}");
        assert_eq!(
            posix(ExitPolicy::All, list),
            None,
            "`set -e` covers the list"
        );

        // A trailing separator is not a second statement.
        assert_eq!(posix(ExitPolicy::Pipefail, "cargo test;"), None);
        assert_eq!(posix(ExitPolicy::Pipefail, "cargo test\n"), None);
        // Newlines separate statements exactly as `;` does.
        assert!(posix(ExitPolicy::Pipefail, "cargo build\ncargo test").is_some());
    }

    #[test]
    fn a_check_written_as_a_condition_is_reported_rather_than_trusted() {
        for policy in [ExitPolicy::Pipefail, ExitPolicy::All] {
            let limitation =
                posix(policy, "if cargo test; then echo ok; fi").expect("condition is exempt");
            assert!(limitation.contains("an `if`"), "{limitation}");
            assert!(limitation.contains("condition fails"), "{limitation}");

            for command in [
                "while cargo test; do echo again; done",
                "until cargo test; do sleep 1; done",
            ] {
                let loop_ = posix(policy, command).expect("condition is exempt");
                assert!(loop_.contains("condition fails"), "{command}: {loop_}");
            }

            let case = posix(policy, "case \"$1\" in build) cargo build ;; esac")
                .expect("no pattern need match");
            assert!(case.contains("a `case`"), "{case}");
        }

        // A construct whose author re-raised the failure already reports it.
        for command in [
            "if ! cargo test; then exit 1; fi",
            "if cargo test; then echo ok; else exit 1; fi",
            "case \"$1\" in build) cargo build ;; *) exit 2 ;; esac",
        ] {
            assert_eq!(posix(ExitPolicy::All, command), None, "{command}");
        }
    }

    #[test]
    fn a_loop_is_reported_when_the_policy_cannot_stop_inside_its_body() {
        let each = "for crate in zuno-goal zuno-tools; do cargo test -p $crate; done";
        let limitation = posix(ExitPolicy::Pipefail, each).expect("only the last iteration");
        assert!(limitation.contains("last iteration"), "{limitation}");
        // `set -e` stops in the body at the first failure, so the loop's status is honest.
        assert_eq!(posix(ExitPolicy::All, each), None);

        let guarded = "for crate in zuno-goal zuno-tools; do cargo build -p $crate && cargo test \
                       -p $crate; done";
        let limitation = posix(ExitPolicy::All, guarded).expect("`set -e` exempts the list");
        assert!(limitation.contains("`&&` or `||`"), "{limitation}");
    }

    #[test]
    fn a_guarded_list_before_the_last_statement_is_reported_under_all() {
        let command = "cargo build && cargo test; echo done";
        let limitation = posix(ExitPolicy::All, command).expect("`set -e` exempts the list");
        assert!(
            limitation.contains("cargo build && cargo test"),
            "{limitation}"
        );
        assert!(
            limitation.contains("before its last statement"),
            "{limitation}"
        );
        // Under `pipefail` the same text is already covered by the statement count.
        let counted = posix(ExitPolicy::Pipefail, command).expect("only the last statement");
        assert!(counted.contains("more than one statement"), "{counted}");

        // A list that *is* the last statement is the status the shell reports.
        assert_eq!(
            posix(ExitPolicy::All, "cargo build; cargo test && echo ok"),
            None
        );
    }

    #[test]
    fn powershell_admits_that_only_the_last_statement_is_re_raised() {
        let single = text_limitation(
            CommandShellKind::PowerShell,
            ExitPolicy::All,
            "cargo test | Select-Object -First 5",
        );
        assert_eq!(single, None);

        let limitation = text_limitation(
            CommandShellKind::PowerShell,
            ExitPolicy::All,
            "cargo build; cargo test",
        )
        .expect("only the last statement");
        assert!(limitation.contains("$LASTEXITCODE"), "{limitation}");
    }

    #[test]
    fn powershell_reports_a_pipeline_the_single_re_raise_cannot_carry() {
        let piped = text_limitation(
            CommandShellKind::PowerShell,
            ExitPolicy::All,
            "cargo test | findstr FAIL",
        )
        .expect("only the last native command");
        assert!(piped.contains("$LASTEXITCODE"), "{piped}");
        assert!(piped.contains("native command"), "{piped}");

        // A cmdlet's failure is a terminating error under `$ErrorActionPreference`,
        // so a pipeline with one native command at most is left alone.
        for command in [
            "cargo test | Select-Object -First 5",
            "Get-Content build.log | Select-String fail",
        ] {
            assert_eq!(
                text_limitation(CommandShellKind::PowerShell, ExitPolicy::All, command),
                None,
                "{command}"
            );
        }
    }

    #[test]
    fn an_unparseable_command_leaves_the_configuration_s_verdict_standing() {
        // Nothing here is a construct this module knows, and a parser that produces
        // an error tree must not be read as evidence against the command.
        assert_eq!(
            posix(ExitPolicy::Pipefail, "cargo test 'unterminated"),
            None
        );
        assert_eq!(posix(ExitPolicy::Pipefail, ""), None);
    }
}

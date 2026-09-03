//! Resource spellings the matcher accepts, and the canonical spelling it prefers.
//!
//! A rule is written once by a human; the resource it is matched against arrives
//! from wherever the call was made. The same command and the same file therefore
//! reach this crate under several spellings, and a matcher that only compared the
//! raw spelling was bypassable: `{"shell": {"rm -rf*": "deny"}}` stopped
//! `rm -rf x` but not `rm  -rf x`, `'rm' -rf x`, `\rm -rf x`, `/bin/rm -rf x`, or
//! `command rm -rf x`. Quoting is not only something that surrounds a word either:
//! a shell removes it per character and before it looks up the program, so
//! `'r'm`, `r"m"`, `rm""` and — wherever `\` is an escape rather than a path
//! separator — `r\m` all invoke `rm` too.
//!
//! # Canonical spellings
//!
//! * A **shell** resource is one command line with a single space between tokens,
//!   the program token reduced to the word the host's own shell builds from it, and
//!   a leading `command` builtin removed. See [`canonical_shell_resource`].
//! * A **path** resource is forward-slashed, with no `.` segments, no `./` prefix,
//!   no repeated separators and no trailing separator. Inside the workspace it is
//!   **workspace-relative**, which is the spelling the file tools already derive
//!   (`crates/zuno-tools/src/read/support.rs`); outside the workspace it is
//!   absolute. See [`canonical_path_resource`].
//!
//! # Why a deny widens further than an allow
//!
//! Normalizations that preserve *which* program or file is named — whitespace
//! between tokens, quote removal, a `\` the host's own shell removes, a `./`
//! prefix, separators, the `command` builtin where the host's shell has that
//! builtin — apply to every rule whatever its action, because the two spellings
//! denote the same thing.
//!
//! Normalizations that *discard* identity apply to `deny` only:
//!
//! * reducing `/bin/rm` to `rm`, which drops which executable was named,
//! * reading the program token under a dialect the host's shell may not be —
//!   cmd's `^`, PowerShell's backtick, bash/zsh `$'...'` and bash `$"..."`, or the
//!   POSIX `\` escape on a Windows host — which guesses the interpreter,
//! * reading a program word that contains whitespace, which can re-partition the
//!   rule: the matcher compares one flattened command line, so admitting the word
//!   `/bin/rm -rf` would let allow `/bin/rm -rf *` govern `"/bin/rm -rf" /` — a
//!   file literally named `/bin/rm -rf`, which the agent can plant itself. Being
//!   path-shaped does not make it safe, so no shape of it reaches an `allow`,
//! * dropping the `command` word on a host whose shell has no such builtin, where
//!   `command rm -rf x` runs a program named `command` instead, and
//! * relating an absolute path to a relative one, which this crate cannot do
//!   exactly because it is not told the workspace root and a host may run
//!   sessions in several directories at once.
//!
//! Widening a deny over-refuses, which fails safe. Widening an allow the same way
//! would let an allow rule for `rm out` cover `/tmp/attacker/rm out`, or an allow
//! rule for `/tmp/out/x` cover a workspace-relative `x` in some other directory.
//! A grant has to keep naming exactly what the user named, so the remaining gap
//! is closed by making the callers agree on the canonical spelling instead — see
//! [`canonical_path_resource`].
//!
//! Which escape character the line is written with is a property of the shell, not
//! of this crate: `\` escapes under every POSIX shell and separates path segments
//! under cmd and PowerShell, so `r\m.exe` is `rm.exe` on one host and a relative
//! path to a different file on the other. The **identity** reading therefore
//! follows the host default (`HostShell`) and a `deny` reads the token under every
//! dialect regardless (`shell_deny_spellings`), which keeps a prohibition portable
//! without letting a grant guess.
//!
//! # A program this crate cannot spell
//!
//! `$PROG`, `` `which rm` ``, `/bin/r?` and `$'\x72\x6d'` name a program only the
//! shell can work out. Reporting "no match" for those would turn an explicit
//! prohibition into the catch-all, so they are reported as *unresolvable* instead
//! and a `deny` is retried with the program the rule itself names, in
//! `Spellings::matches`. That over-refuses, which is the direction that fails safe;
//! an `allow` never sees them.
//!
//! An **unquoted** run goes further than that, because the shell word-splits its
//! result: `rm${IFS}-rf${IFS}/tmp/build`, `$(echo rm -rf /tmp/build)` and the
//! backtick spelling are one token here and supply the program *and* its arguments
//! — measured, bash and dash both run them as `rm` with `-rf /tmp/build`. When no
//! argument token follows such a run, nothing about the line is knowable and a
//! `deny` matches whatever it is written as, rather than assuming the arguments are
//! still visible. Where argument tokens *do* follow, they are still the words the
//! shell will pass, so the retry above applies and `$PROG status` stays an ask
//! under `rm -rf*`. A program token *no* dialect can read at all — an unterminated
//! quote, a trailing escape, an empty word — is unresolvable for the same reason and
//! fails closed the same way; no shell runs those lines, so the cost is a command
//! that could not have executed.

use crate::types::Rule;
use crate::wildcard::wildcard_match;
use zuno_config::schema::permission::PermissionAction;

/// Permission keys whose resource is a shell command line.
const SHELL_PERMISSIONS: [&str; 1] = ["shell"];

/// Permission keys whose resource is a filesystem path.
///
/// `lsp` is here because its resource is one too: `zuno-lsp` resolves the requested
/// file and names it relative to the worktree that contains the session, falling back
/// to the resolved absolute path where the two roots do not nest. Both spellings
/// therefore reach this crate for the same file, which is exactly the situation the
/// path treatment exists for — without it an `lsp` deny written the way a repository
/// is read, `"src/main.rs"`, misses the absolute spelling and degrades to an ask.
///
/// Membership is what a rule may match, not what the tool may touch: an `lsp` request
/// is confined by `zuno-lsp` before it reaches a rule at all.
const PATH_PERMISSIONS: [&str; 5] = ["read", "edit", "write", "list", "lsp"];

/// What kind of thing the resource of one permission is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceKind {
    Shell,
    Path,
    /// A URL, a query, an agent name, a raw argument object: matched verbatim.
    Opaque,
}

impl ResourceKind {
    fn of(permission: &str) -> Self {
        if SHELL_PERMISSIONS.contains(&permission) {
            return Self::Shell;
        }
        if PATH_PERMISSIONS.contains(&permission) {
            return Self::Path;
        }
        Self::Opaque
    }
}

/// Every spelling of one requested resource, prepared once per evaluation.
pub(crate) struct Spellings {
    kind: ResourceKind,
    permission: String,
    /// Spellings that denote exactly the same resource, in match order.
    identity: Vec<String>,
    /// Extra spellings only a `deny` rule is allowed to match.
    deny_only: Vec<String>,
    /// What a shell command whose program token names a program only the shell can
    /// resolve still tells us. `None` in the ordinary case.
    unresolved: Option<UnresolvedProgram>,
}

impl Spellings {
    pub(crate) fn new(permission: &str, resource: &str) -> Self {
        Self::for_host(permission, resource, HostShell::current())
    }

    fn for_host(permission: &str, resource: &str, host: HostShell) -> Self {
        let kind = ResourceKind::of(permission);
        let mut identity = vec![resource.to_owned()];
        match kind {
            ResourceKind::Shell => {
                push_new(&mut identity, canonical_shell_resource_for(resource, host));
            }
            ResourceKind::Path => push_new(&mut identity, canonical_path_resource(resource)),
            ResourceKind::Opaque => {}
        }
        let mut deny_only = Vec::new();
        for spelling in deny_only_spellings(kind, resource) {
            if !identity.contains(&spelling) {
                push_new(&mut deny_only, spelling);
            }
        }
        let unresolved = (kind == ResourceKind::Shell)
            .then(|| unresolved_program(resource))
            .flatten();
        Self {
            kind,
            permission: permission.to_owned(),
            identity,
            deny_only,
            unresolved,
        }
    }

    /// Whether `rule` governs this request.
    pub(crate) fn matches(&self, rule: &Rule) -> bool {
        if !wildcard_match(&self.permission, &rule.permission) {
            return false;
        }
        if self.matches_pattern(&rule.pattern) {
            return true;
        }
        if rule.action != PermissionAction::Deny {
            return false;
        }
        if self
            .deny_only
            .iter()
            .any(|spelling| wildcard_match(spelling, &rule.pattern))
        {
            return true;
        }
        // A program token this crate cannot resolve without running the shell — an
        // expansion, a glob, an encoded `$'\x72'` escape — must not degrade into a
        // silent non-match, because that turns an explicit prohibition into the
        // catch-all.
        match &self.unresolved {
            // The whole line is one unquoted run whose result the shell word-splits
            // into the program *and* its arguments, so which command runs is not
            // knowable at all and the denied one is among the possibilities. Refusing
            // every pattern over-refuses, which is the direction that fails safe;
            // assuming the arguments were still visible is what let the deny miss.
            Some(UnresolvedProgram::WholeLine) => return true,
            // The argument tokens are still the words the shell will pass, so the
            // deny is retried with the program the rule itself names and the
            // arguments still have to fit the rule.
            Some(UnresolvedProgram::Arguments(arguments)) => {
                let pattern_tokens = shell_tokens(&rule.pattern);
                if let Some(program) = pattern_tokens.first()
                    && wildcard_match(&command_line(program, arguments), &rule.pattern)
                {
                    return true;
                }
            }
            None => {}
        }
        // A deny written with an absolute path also covers the relative spelling of
        // the same file. The workspace root is unknown here, so the pattern is
        // shortened at segment boundaries and the all-wildcard tail is dropped: an
        // absolute deny must not degrade into "deny every relative path".
        self.kind == ResourceKind::Path
            && segment_suffixes(&canonical_path_resource(&rule.pattern))
                .into_iter()
                .filter(|suffix| has_literal_segment(suffix))
                .any(|suffix| self.matches_pattern(&suffix))
    }

    fn matches_pattern(&self, pattern: &str) -> bool {
        self.identity
            .iter()
            .any(|spelling| wildcard_match(spelling, pattern))
    }
}

fn deny_only_spellings(kind: ResourceKind, resource: &str) -> Vec<String> {
    let mut spellings = Vec::new();
    match kind {
        ResourceKind::Shell => {
            for spelling in shell_deny_spellings(resource) {
                push_new(&mut spellings, spelling);
            }
        }
        ResourceKind::Path => {
            let resolved = normalized_path(resource, true);
            push_new(&mut spellings, resolved.clone());
            if is_absolute(&resolved) {
                for suffix in segment_suffixes(&resolved) {
                    push_new(&mut spellings, suffix);
                }
            }
        }
        ResourceKind::Opaque => {}
    }
    spellings
}

fn push_new(spellings: &mut Vec<String>, spelling: String) {
    if !spelling.is_empty() && !spellings.contains(&spelling) {
        spellings.push(spelling);
    }
}

/// The canonical spelling of a shell resource.
///
/// One space between tokens, the program token reduced to the word the host's own
/// shell builds from it, and a leading `command` builtin dropped. Argument tokens
/// keep their own quoting, because a rule that mentions an argument means that
/// argument as the caller wrote it.
#[must_use]
pub fn canonical_shell_resource(command: &str) -> String {
    canonical_shell_resource_for(command, HostShell::current())
}

fn canonical_shell_resource_for(command: &str, host: HostShell) -> String {
    let written = shell_tokens(command);
    // Dropping `command` is only identity-preserving where it *is* the builtin. cmd
    // and PowerShell have no such builtin and search the current directory first, so
    // dropping the word there would let a grant for `rm -rf *` cover whatever
    // `command.exe` an attacker planted. A deny still drops it under every dialect.
    let tokens = if host.has_command_builtin() {
        without_command_builtin(&written, |token| {
            names_command_builtin(token, host.syntax())
        })
    } else {
        &written
    };
    let Some((program, arguments)) = tokens.split_first() else {
        return command.trim().to_owned();
    };
    let program = identity_program(program, host).unwrap_or_else(|| program.clone());
    command_line(&program, arguments)
}

/// The canonical spelling of a path resource.
///
/// Forward slashes, no `.` segments, no `./` prefix, no repeated separators and no
/// trailing separator. `..` is **not** resolved here: a lexical `..` can leave the
/// directory a symlink pointed at, so resolving it is only used to widen a deny.
#[must_use]
pub fn canonical_path_resource(resource: &str) -> String {
    normalized_path(resource, false)
}

fn normalized_path(resource: &str, resolve_parents: bool) -> String {
    let slashed = resource.replace('\\', "/");
    let (root, rest) = split_root(&slashed);
    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." if resolve_parents => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else if root.is_empty() {
                    segments.push(segment);
                }
            }
            segment => segments.push(segment),
        }
    }
    let joined = segments.join("/");
    if joined.is_empty() {
        if root.is_empty() {
            // `.` is how the file tools spell the workspace root itself.
            return if slashed.is_empty() {
                String::new()
            } else {
                ".".to_owned()
            };
        }
        return root.to_owned();
    }
    format!("{root}{joined}")
}

/// Split a leading `/` or `C:/` off a forward-slashed path.
fn split_root(value: &str) -> (&str, &str) {
    if let Some(rest) = value.strip_prefix('/') {
        return ("/", rest);
    }
    if drive_root(value).is_some() {
        return value.split_at(3);
    }
    ("", value)
}

fn drive_root(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/')
        .then(|| &value[..3])
}

fn is_absolute(value: &str) -> bool {
    value.starts_with('/') || drive_root(value).is_some()
}

/// Every tail of `value` that starts at a segment boundary, longest first.
///
/// `/etc/ssh/config` yields `etc/ssh/config`, `ssh/config`, `config`.
fn segment_suffixes(value: &str) -> Vec<String> {
    value
        .char_indices()
        .filter(|(_, character)| *character == '/')
        .filter_map(|(index, _)| value.get(index + 1..))
        .filter(|suffix| !suffix.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a pattern still names something, rather than only wildcards.
fn has_literal_segment(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|character| !matches!(character, '*' | '?' | '/'))
}

/// The command lines only a `deny` rule is matched against.
///
/// A deny has to hold for whichever interpreter runs the line and for whatever the
/// program token turns out to name, so this widens past the canonical spelling in
/// two ways a grant must not follow: it reads the program under **every** dialect
/// in [`DENY_SYNTAXES`], including the ones the host's shell may not be, and it
/// reduces a program path to its file name (`/bin/rm -rf x` becomes `rm -rf x`).
/// Both drop information, and both can only refuse more than was asked, which
/// fails safe.
fn shell_deny_spellings(command: &str) -> Vec<String> {
    let tokens = shell_tokens(command);
    let tokens = without_command_builtin(&tokens, |token| {
        DENY_SYNTAXES
            .iter()
            .any(|syntax| names_command_builtin(token, *syntax))
    });
    let Some((program, arguments)) = tokens.split_first() else {
        return Vec::new();
    };
    let mut programs = Vec::new();
    for syntax in DENY_SYNTAXES {
        if let Some(ProgramWord::Literal(word)) = program_word(program, syntax) {
            push_new(&mut programs, word);
        }
    }
    // A Windows path separator survives every reading above, so the raw token is
    // what `.\rm.exe` or `C:\bin\rm.exe` has to be reduced from.
    push_new(&mut programs, program.clone());
    let mut spellings = Vec::new();
    for program in &programs {
        push_new(&mut spellings, command_line(program, arguments));
        if let Some(base) = program_basename(program) {
            push_new(&mut spellings, command_line(base, arguments));
        }
    }
    spellings
}

/// What one command whose program token names no knowable program still tells us.
///
/// A word only the shell can work out is reported rather than guessed. `None` when
/// at least one dialect reduces the token to a literal word and none of them reports
/// an unresolvable one, which is the ordinary case. Any dialect reporting
/// [`ProgramWord::Unresolved`] is enough: a deny that cannot tell which program was
/// named has to assume the worst.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UnresolvedProgram {
    /// The program token names no knowable program, and these are the argument tokens
    /// the shell will pass after it.
    Arguments(Vec<String>),
    /// Nothing about the line is knowable: either it is one token whose *unquoted*
    /// run the shell resolves, so the result is word-split into the program *and* its
    /// arguments, or no dialect can read the program token at all.
    WholeLine,
}

fn unresolved_program(command: &str) -> Option<UnresolvedProgram> {
    let tokens = shell_tokens(command);
    let tokens = without_command_builtin(&tokens, |token| {
        DENY_SYNTAXES
            .iter()
            .any(|syntax| names_command_builtin(token, *syntax))
    });
    let (program, arguments) = merge_open_substitution(tokens)?;
    let readings = DENY_SYNTAXES.map(|syntax| program_word(&program, syntax));
    // No dialect can read the token at all: an unterminated quote, a trailing escape
    // or an empty program word. Which program the line names is then not knowable
    // either, and a silent non-match is exactly what turns a prohibition into the
    // catch-all, so the same fail-closed reading applies. `rm'${IFS}-rf${IFS}/tmp/build`
    // is measured as a syntax error under bash, dash, zsh and PowerShell, so refusing
    // it costs a command that could not have run.
    if readings.iter().all(Option::is_none) {
        return Some(UnresolvedProgram::WholeLine);
    }
    if !readings
        .iter()
        .any(|reading| matches!(reading, Some(ProgramWord::Unresolved)))
    {
        return None;
    }
    // An unquoted expansion, substitution or glob is word-split, so when no argument
    // token follows it the shell builds the program *and* every argument out of text
    // this crate cannot resolve. Measured with a fake `rm` on `PATH`: bash and dash
    // both run `rm${IFS}-rf${IFS}/tmp/build`, `$(echo rm -rf /tmp/build)` and the
    // backtick spelling as `rm` with `-rf /tmp/build`, and all of them are one token
    // here. Reporting only the program as unresolvable let the retry below compare
    // the bare rule program (`rm`) against `rm -rf*` and match nothing.
    if arguments.is_empty() && splits_into_words(&program) {
        return Some(UnresolvedProgram::WholeLine);
    }
    Some(UnresolvedProgram::Arguments(arguments))
}

/// Whether `token` resolves a run *outside* quotes, so the shell word-splits it.
///
/// Only an unquoted expansion, command substitution or glob is subject to word
/// splitting and globbing, and that is what decides whether the result supplies the
/// program alone or the program together with its arguments: `"$PROG"` is one word
/// whatever `PROG` holds, while `$PROG`, `${IFS}`, `$(...)`, `` `...` `` and `r?`
/// become as many words as their result contains. `$'...'` and `$"..."` are quoting,
/// not expansion, so a run they open never splits.
fn splits_into_words(token: &str) -> bool {
    let mut characters = without_verbatim_prefix(token).chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            // Outside quotes a backslash escapes exactly one character, so `\$` is a
            // literal dollar and `\*` is a literal star.
            '\\' => {
                characters.next();
            }
            '$' if matches!(characters.peek(), Some('\'' | '"')) => {
                let Some(quote) = characters.next() else {
                    return false;
                };
                skip_quoted_run(&mut characters, quote);
            }
            '\'' | '"' => skip_quoted_run(&mut characters, character),
            '`' | '*' | '?' | '[' => return true,
            '$' if characters.peek().is_some_and(|next| opens_expansion(*next)) => return true,
            _ => {}
        }
    }
    false
}

/// Skip to the end of a quoted run, leaving the iterator after its delimiter.
fn skip_quoted_run(characters: &mut std::iter::Peekable<std::str::Chars<'_>>, delimiter: char) {
    while let Some(character) = characters.next() {
        if character == delimiter {
            return;
        }
        // A single-quoted run is literal; a double-quoted one honours `\`.
        if delimiter != '\'' && character == '\\' {
            characters.next();
        }
    }
}

/// Whether what follows an unquoted `$` starts an expansion rather than a literal.
fn opens_expansion(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(
            character,
            '_' | '{' | '(' | '@' | '*' | '#' | '?' | '$' | '!' | '-'
        )
}

/// The program token and the arguments after it, with a command substitution the
/// program position opens rejoined into one token.
///
/// `shell_tokens` splits on whitespace, which cuts `` `which rm` `` and
/// `$(which rm)` in half and left the substitution's own words looking like
/// arguments. Which program runs is decided by the whole substitution, so a deny has
/// to see it whole; otherwise `` `which rm` -rf /tmp/build`` was compared against
/// `rm -rf*` with `` rm` `` as its first argument and matched nothing.
/// The scan is carried across tokens rather than restarted, because the text is
/// model-controlled and this runs synchronously inside authorization: re-reading the
/// accumulated program token once per argument was quadratic, and 120 KB of command
/// line took 9.6 s here (1 KB took 1.1 ms) while holding the async task.
fn merge_open_substitution(tokens: &[String]) -> Option<(String, Vec<String>)> {
    let (first, arguments) = tokens.split_first()?;
    let mut program = first.clone();
    let mut scan = SubstitutionScan::default();
    scan.feed(first);
    let mut rest = arguments;
    while scan.is_open() {
        let Some((next, tail)) = rest.split_first() else {
            break;
        };
        program.push(' ');
        program.push_str(next);
        scan.feed(" ");
        scan.feed(next);
        rest = tail;
    }
    Some((program, rest.to_vec()))
}

/// How much of a backtick pair or a `$(` is still unclosed, carried between tokens.
#[derive(Debug, Clone, Copy, Default)]
struct SubstitutionScan {
    backticks: usize,
    depth: usize,
    /// Whether the previous character was an unconsumed `\`, which can happen at a
    /// token boundary and has to survive into the next chunk.
    escaped: bool,
}

impl SubstitutionScan {
    /// Advance the scan across one more chunk of the command line.
    fn feed(&mut self, text: &str) {
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            if self.escaped {
                self.escaped = false;
                continue;
            }
            match character {
                '\\' => self.escaped = true,
                '`' => self.backticks += 1,
                '$' if characters.peek() == Some(&'(') => {
                    characters.next();
                    self.depth += 1;
                }
                ')' if self.depth > 0 => self.depth -= 1,
                _ => {}
            }
        }
    }

    const fn is_open(self) -> bool {
        self.backticks % 2 == 1 || self.depth > 0
    }
}

/// The file name a program path ends with, when it names one at all.
fn program_basename(program: &str) -> Option<&str> {
    program
        .rsplit(['/', '\\'])
        .next()
        .filter(|base| !base.is_empty() && *base != program)
}

/// One command line from a program spelling and the argument tokens as written.
fn command_line(program: &str, arguments: &[String]) -> String {
    let mut line = program.to_owned();
    for argument in arguments {
        line.push(' ');
        line.push_str(argument);
    }
    line
}

/// Split a command line into tokens on spaces and tabs, honouring quotes.
///
/// Newlines are deliberately not separators: `a\nb` is two commands, and joining
/// them into one line would invent a command nobody ran.
fn shell_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            current.push(character);
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            current.push(character);
            continue;
        }
        if matches!(character, ' ' | '\t') && quote.is_none() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(character);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Drop a leading `command` builtin, which runs the program without its alias.
fn without_command_builtin(tokens: &[String], names_builtin: impl Fn(&str) -> bool) -> &[String] {
    let mut rest = tokens;
    while let Some((first, tail)) = rest.split_first() {
        if tail.is_empty() || !names_builtin(first) {
            break;
        }
        rest = tail;
    }
    rest
}

/// Whether one reading of `token` names the `command` builtin.
fn names_command_builtin(token: &str, syntax: WordSyntax) -> bool {
    matches!(program_word(token, syntax), Some(ProgramWord::Literal(word)) if word == "command")
}

/// Which shell's escaping the identity reading of a program token follows.
///
/// This crate is not told which interpreter will run the line, and the answer
/// changes what a token *names*: `\` escapes under every POSIX shell and separates
/// path segments under cmd and PowerShell, so `r\m.exe` is another spelling of
/// `rm.exe` on one host and a relative path to a different file on the other.
/// Reading it the POSIX way everywhere would let an allow rule for `rm.exe *` grant
/// a planted `.\r\m.exe` on Windows, so the identity reading follows the host and
/// the portable half of the guarantee is carried by [`shell_deny_spellings`], which
/// reads every dialect for a `deny`. `crate::wildcard` forks on the same host for
/// the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostShell {
    Posix,
    Windows,
}

impl HostShell {
    /// The reading this build's host uses. Written with `cfg!` rather than two
    /// `#[cfg]` bodies so that both variants exist in every build and the tests below
    /// exercise the Windows reading on a Linux host as well.
    const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }

    /// Whether this host's default shell has the POSIX `command` builtin.
    const fn has_command_builtin(self) -> bool {
        matches!(self, Self::Posix)
    }

    /// The one reading that keeps naming the same file on this host.
    const fn syntax(self) -> WordSyntax {
        match self {
            // `$'...'` is bash and zsh only — dash reads `r$'m'` as the program
            // `r$m` — so it is a dialect guess even on a POSIX host and stays on
            // the deny side.
            Self::Posix => WordSyntax::Posix,
            // cmd escapes with `^` and PowerShell with a backtick. Choosing between
            // them is a guess, so a Windows identity reading removes quotes only.
            Self::Windows => WordSyntax::Quotes,
        }
    }
}

/// The dialects a `deny` reads a program token under, widest coverage first.
///
/// A prohibition has to hold whichever shell the host is configured with, so every
/// reading is tried even though at most one of them is what actually ran.
const DENY_SYNTAXES: [WordSyntax; 4] = [
    WordSyntax::Posix,
    WordSyntax::PosixDollar,
    WordSyntax::Windows,
    WordSyntax::Quotes,
];

/// Which quoting and escaping one reading of a program token reverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordSyntax {
    /// `'` and `"` only. Every shell removes those and neither changes which file
    /// is named, so this reading is safe on every host.
    Quotes,
    /// `Quotes` plus the POSIX `\` escape.
    Posix,
    /// `Posix` plus bash/zsh `$'...'` ANSI-C quoting and bash `$"..."` locale
    /// quoting. Measured: bash runs `r$'m' -rf x` as `rm`, zsh runs `$'...'` but
    /// not `$"..."`, and dash runs neither.
    PosixDollar,
    /// `Quotes` plus cmd's `^` and PowerShell's backtick.
    Windows,
}

impl WordSyntax {
    /// Whether `character` escapes the next character outside quotes.
    const fn escapes(self, character: char) -> bool {
        match self {
            Self::Quotes => false,
            Self::Posix | Self::PosixDollar => character == '\\',
            Self::Windows => matches!(character, '^' | '`'),
        }
    }

    /// Whether `character` escapes `next` inside a double-quoted run.
    ///
    /// A POSIX shell keeps a backslash there literally unless it guards a character
    /// that could end the string or start an expansion.
    fn escapes_inside_quotes(self, character: char, next: Option<char>) -> bool {
        match self {
            Self::Quotes => false,
            Self::Posix | Self::PosixDollar => {
                character == '\\' && matches!(next, Some('$' | '`' | '"' | '\\'))
            }
            Self::Windows => self.escapes(character) && next.is_some(),
        }
    }

    /// Whether `$'...'` and `$"..."` are quoting in this dialect.
    const fn dollar_quotes(self) -> bool {
        matches!(self, Self::PosixDollar)
    }
}

/// What one reading makes of a program token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProgramWord {
    /// The exact literal word the shell would look the program up by.
    Literal(String),
    /// The token names a program only the shell can work out: a parameter or
    /// command substitution, a glob, or an encoded `$'\x72'` escape. Reporting this
    /// rather than a literal is what keeps the deny side from silently missing.
    Unresolved,
}

/// The program a token names for **every** rule, whatever its action.
///
/// Quoting and escaping a program name exists only to bypass the shell's alias and
/// function lookup, so `rm`, `'r'm`, `r"m"`, `rm""` and — where `\` is an escape —
/// `r\m` all name the same program and every rule sees the reduction. It stops
/// short of a word that is not one token any more: rewriting `rm" "-rf` to `rm -rf`
/// would claim the line invoked `rm` when the shell looked for a program whose name
/// contains a space.
///
/// A word that contains whitespace is **never** the identity spelling, whatever it
/// looks like. The matcher compares one flattened command line, so a reduced word
/// with a space in it can line up with the rule's own program/argument boundary and
/// re-partition the rule: allow `/bin/rm -rf *` would govern `"/bin/rm -rf" /`, where
/// the file the shell executes is the one literally named `/bin/rm -rf` and not the
/// one the rule names, and allow `./tool.sh *` would govern `"./tool.sh evil"`. Being
/// path-shaped does not make that safe — both of those are paths, and both are
/// plantable in the workspace the agent can already write (`cp evil.sh "./tool.sh
/// evil"`). The whitespace reading stays on the deny side, where over-refusing is the
/// direction that fails safe.
fn identity_program(token: &str, host: HostShell) -> Option<String> {
    let ProgramWord::Literal(word) = program_word(token, host.syntax())? else {
        return None;
    };
    if word.contains([' ', '\t']) {
        return None;
    }
    Some(word)
}

/// The word one dialect builds from `token` once its quoting is removed.
///
/// `None` for a token that reduces to nothing or that no shell would accept: an
/// unterminated quote or a trailing escape means the word was cut off, and reducing
/// it would invent a program nobody named.
fn program_word(token: &str, syntax: WordSyntax) -> Option<ProgramWord> {
    let mut word = String::new();
    let mut characters = token.chars().peekable();
    while let Some(character) = characters.next() {
        // `$'...'` is ANSI-C quoting and `$"..."` is locale quoting. Both are
        // quoting rather than expansion, so `r$'m'` is one more spelling of `rm`.
        if syntax.dollar_quotes()
            && character == '$'
            && let Some(&quote @ ('\'' | '"')) = characters.peek()
        {
            characters.next();
            let run = if quote == '\'' {
                ansi_c_run(&mut characters)?
            } else {
                ProgramWord::Literal(quoted_run(&mut characters, '"', syntax)?)
            };
            match run {
                ProgramWord::Literal(text) => word.push_str(&text),
                ProgramWord::Unresolved => return Some(ProgramWord::Unresolved),
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            word.push_str(&quoted_run(&mut characters, character, syntax)?);
            continue;
        }
        if syntax.escapes(character) {
            word.push(characters.next()?);
            continue;
        }
        word.push(character);
    }
    if word.is_empty() {
        return None;
    }
    Some(if needs_the_shell(&word) {
        ProgramWord::Unresolved
    } else {
        ProgramWord::Literal(word)
    })
}

/// The literal one quoted run contributes, consuming its closing delimiter.
///
/// `None` when the run is never closed, or when a trailing escape cuts it off.
fn quoted_run(
    characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
    delimiter: char,
    syntax: WordSyntax,
) -> Option<String> {
    let mut text = String::new();
    while let Some(character) = characters.next() {
        if character == delimiter {
            return Some(text);
        }
        // A single-quoted run is literal, escape characters included.
        if delimiter != '\'' && syntax.escapes_inside_quotes(character, characters.peek().copied())
        {
            text.push(characters.next()?);
            continue;
        }
        text.push(character);
    }
    None
}

/// The literal a bash/zsh `$'...'` run contributes.
///
/// The unambiguous single-character escapes are reversed exactly. `\xHH`, `\uHHHH`,
/// `\UHHHHHHHH`, an octal `\NNN` and `\cX` spell a character this crate would have
/// to decode; rather than guess, the run is reported [`ProgramWord::Unresolved`] so
/// the deny side fails closed. `None` when the run is never closed.
fn ansi_c_run(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<ProgramWord> {
    let mut text = String::new();
    let mut unresolved = false;
    while let Some(character) = characters.next() {
        if character == '\'' {
            return Some(if unresolved {
                ProgramWord::Unresolved
            } else {
                ProgramWord::Literal(text)
            });
        }
        if character != '\\' {
            text.push(character);
            continue;
        }
        match characters.next()? {
            escaped @ ('\\' | '\'' | '"' | '?') => text.push(escaped),
            'a' => text.push('\u{7}'),
            'b' => text.push('\u{8}'),
            'e' | 'E' => text.push('\u{1b}'),
            'f' => text.push('\u{c}'),
            'n' => text.push('\n'),
            'r' => text.push('\r'),
            't' => text.push('\t'),
            'v' => text.push('\u{b}'),
            _ => unresolved = true,
        }
    }
    None
}

/// Whether a reduced word still contains something only the shell can resolve.
///
/// A parameter or command substitution and a glob are decided at run time, so which
/// program the token names is not knowable here. `bash` really does run `/bin/r?`
/// as `/bin/rm`, and the matcher reads `?` in a resource as a literal, so treating
/// this as an ordinary word is exactly the silent non-match a deny must not make.
fn needs_the_shell(word: &str) -> bool {
    // `\\?\` and `\\.\` spell a Windows root, so their `?` is not a glob.
    without_verbatim_prefix(word).contains(['$', '`', '*', '?', '['])
}

/// The word with a Windows verbatim or device prefix removed.
fn without_verbatim_prefix(word: &str) -> &str {
    let mut characters = word.chars();
    let separator = |character: Option<char>| matches!(character, Some('/' | '\\'));
    if separator(characters.next())
        && separator(characters.next())
        && matches!(characters.next(), Some('?' | '.'))
        && separator(characters.next())
    {
        return &word[4..];
    }
    word
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, action: PermissionAction) -> Rule {
        Rule {
            permission: "shell".to_owned(),
            pattern: pattern.to_owned(),
            action,
        }
    }

    fn governs(resource: &str, host: HostShell, rule: &Rule) -> bool {
        Spellings::for_host("shell", resource, host).matches(rule)
    }

    #[test]
    fn a_windows_host_never_grants_a_backslash_respelling_of_the_allowed_program() {
        // On Windows `\` separates path segments, so `r\m.exe` is a relative path to
        // a *different*, plantable executable rather than another spelling of
        // `rm.exe`. Reading it the POSIX way everywhere turned a standing grant into
        // auto-approval for `.\r\m.exe`, `.\pyt\hon.exe` and `.\n\pm.cmd`.
        for (resource, pattern) in [
            (r"r\m.exe -rf C:\", "rm.exe *"),
            (r"pyt\hon.exe -c evil", "python.exe *"),
            (r"n\pm.cmd install evil", "npm.cmd *"),
        ] {
            let allow = rule(pattern, PermissionAction::Allow);
            assert!(
                !governs(resource, HostShell::Windows, &allow),
                "`{pattern}` must not grant `{resource}` on a Windows host"
            );
            assert!(
                governs(resource, HostShell::Posix, &allow),
                "on a POSIX host the shell really does remove the backslash, so \
                 `{resource}` is the program `{pattern}` names"
            );
            assert!(
                governs(
                    resource,
                    HostShell::Windows,
                    &rule(pattern, PermissionAction::Deny)
                ),
                "a deny reads every dialect, so it still stops `{resource}` on Windows"
            );
        }
    }

    #[test]
    fn a_windows_host_keeps_the_program_path_a_rule_names() {
        // Reducing a Windows path with the POSIX rule rewrote
        // `C:\Windows\System32\rm.exe` to `C:WindowsSystem32rm.exe`, which is the
        // defect `zuno-tools`' `static_shell_word` documents and the permissions
        // guide already ships as fixed.
        for (command, expected) in [
            (
                r"C:\Windows\System32\rm.exe -rf C:\build",
                r"C:\Windows\System32\rm.exe -rf C:\build",
            ),
            (r".\rm.exe -rf C:\build", r".\rm.exe -rf C:\build"),
            (r"C:\bin\git.exe  status", r"C:\bin\git.exe status"),
        ] {
            assert_eq!(
                canonical_shell_resource_for(command, HostShell::Windows),
                expected
            );
        }
        assert_eq!(
            canonical_shell_resource_for(
                r"C:\Windows\System32\rm.exe -rf C:\build",
                HostShell::Posix
            ),
            r"C:WindowsSystem32rm.exe -rf C:\build",
            "a POSIX shell really does remove those backslashes, so this is not a bug \
             there — which is why the reading follows the host"
        );
        assert!(
            governs(
                r"C:\bin\git.exe  status",
                HostShell::Windows,
                &rule(r"C:\bin\git.exe status", PermissionAction::Allow)
            ),
            "collapsing the double space must keep the grant a Windows user wrote"
        );
        assert!(
            !governs(
                "\"C:\\Program Files\\Tools\\rm.exe\" -rf C:\\build",
                HostShell::Windows,
                &rule(
                    r"C:\Program Files\Tools\rm.exe -rf *",
                    PermissionAction::Allow
                )
            ),
            "a program word with a space in it cannot reach a grant: the same reduction \
             would let allow `C:\\Program Files\\Tools\\rm.exe -rf *` govern the file \
             literally named `C:\\Program Files\\Tools\\rm.exe -rf`. See \
             `a_program_word_with_a_space_never_reaches_a_grant`."
        );
    }

    #[test]
    fn a_program_word_with_a_space_never_reaches_a_grant() {
        // The matcher compares one flattened command line, so a reduced program word
        // that contains whitespace can line up with the rule's own program/argument
        // boundary and re-partition it. The file the shell executes is then the one
        // literally named `git commit`, `/bin/rm -rf` or
        // `C:\Program Files\Tools\rm.exe -rf` — each of them plantable in a directory
        // the agent can already write — and not the file the rule names. Being
        // path-shaped is orthogonal to that hazard, which is why no shape of a
        // space-containing word is admitted.
        let rows = [
            ("\"git commit\" -m x", "git commit -m *"),
            ("\"/bin/rm -rf\" /", "/bin/rm -rf *"),
            ("'/bin/rm -rf' /", "/bin/rm -rf *"),
            ("\"./tool.sh evil\"", "./tool.sh *"),
            ("\"/usr/bin/git commit\" -m x", "/usr/bin/git commit -m *"),
            (
                "\"C:\\Program Files\\Tools\\rm.exe -rf\" C:\\",
                r"C:\Program Files\Tools\rm.exe -rf *",
            ),
        ];
        for host in [HostShell::Posix, HostShell::Windows] {
            for (resource, pattern) in rows {
                assert!(
                    !governs(resource, host, &rule(pattern, PermissionAction::Allow)),
                    "`{pattern}` must not grant `{resource}` on {host:?}"
                );
                assert!(
                    governs(resource, host, &rule(pattern, PermissionAction::Deny)),
                    "the same respelling is still refused by a deny on {host:?}"
                );
            }
        }
    }

    #[test]
    fn the_host_shell_decides_which_escape_and_which_builtin_the_identity_reading_has() {
        // These rows are host-sensitive, so they are pinned per host here rather than
        // through the public `canonical_shell_resource`, which reads whichever host it
        // was compiled for: asserting the POSIX answer from an integration test would
        // fail on a native Windows run.
        for (command, posix, windows) in [
            // `\` is an escape under every POSIX shell and a path separator under cmd
            // and PowerShell, where `r\m` is a relative path to a different file.
            (r"r\m -rf x", "rm -rf x", r"r\m -rf x"),
            (r"\r\m -rf x", "rm -rf x", r"\r\m -rf x"),
            (r"command r\m -rf x", "rm -rf x", r"command r\m -rf x"),
            (r"\rm -rf x", "rm -rf x", r"\rm -rf x"),
            // cmd and PowerShell have no `command` builtin and search the current
            // directory first, so dropping the word there would let a grant for
            // `rm -rf *` cover a planted `command.exe`.
            ("command rm -rf x", "rm -rf x", "command rm -rf x"),
        ] {
            assert_eq!(
                canonical_shell_resource_for(command, HostShell::Posix),
                posix,
                "POSIX reading of `{command}`"
            );
            assert_eq!(
                canonical_shell_resource_for(command, HostShell::Windows),
                windows,
                "Windows reading of `{command}`"
            );
        }
        assert!(
            !governs(
                "command rm -rf x",
                HostShell::Windows,
                &rule("rm -rf *", PermissionAction::Allow)
            ),
            "a grant for `rm -rf *` must not cover a program named `command`"
        );
        assert!(
            governs(
                "command rm -rf x",
                HostShell::Posix,
                &rule("rm -rf *", PermissionAction::Allow)
            ),
            "where `command` really is the builtin it runs the program the rule names"
        );
        assert!(
            governs(
                "command rm -rf x",
                HostShell::Windows,
                &rule("rm -rf *", PermissionAction::Deny)
            ),
            "a deny still drops the word under every dialect"
        );
    }
}

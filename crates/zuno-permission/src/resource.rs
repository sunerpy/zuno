//! Resource spellings the matcher accepts, and the canonical spelling it prefers.
//!
//! A rule is written once by a human; the resource it is matched against arrives
//! from wherever the call was made. The same command and the same file therefore
//! reach this crate under several spellings, and a matcher that only compared the
//! raw spelling was bypassable: `{"shell": {"rm -rf*": "deny"}}` stopped
//! `rm -rf x` but not `rm  -rf x`, `'rm' -rf x`, `\rm -rf x`, `/bin/rm -rf x`, or
//! `command rm -rf x`.
//!
//! # Canonical spellings
//!
//! * A **shell** resource is one command line with a single space between tokens,
//!   the program token unquoted and unescaped, and a leading `command` builtin
//!   removed. See [`canonical_shell_resource`].
//! * A **path** resource is forward-slashed, with no `.` segments, no `./` prefix,
//!   no repeated separators and no trailing separator. Inside the workspace it is
//!   **workspace-relative**, which is the spelling the file tools already derive
//!   (`crates/zuno-tools/src/read/support.rs`); outside the workspace it is
//!   absolute. See [`canonical_path_resource`].
//!
//! # Why a deny widens further than an allow
//!
//! Normalizations that preserve *which* program or file is named — whitespace,
//! quoting, a leading `\`, a `./` prefix, separators, the `command` builtin —
//! apply to every rule whatever its action, because the two spellings denote the
//! same thing.
//!
//! Normalizations that *discard* identity apply to `deny` only:
//!
//! * reducing `/bin/rm` to `rm`, which drops which executable was named, and
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

use crate::types::Rule;
use crate::wildcard::wildcard_match;
use zuno_config::schema::permission::PermissionAction;

/// Permission keys whose resource is a shell command line.
const SHELL_PERMISSIONS: [&str; 1] = ["shell"];

/// Permission keys whose resource is a filesystem path.
const PATH_PERMISSIONS: [&str; 4] = ["read", "edit", "write", "list"];

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
}

impl Spellings {
    pub(crate) fn new(permission: &str, resource: &str) -> Self {
        let kind = ResourceKind::of(permission);
        let mut identity = vec![resource.to_owned()];
        match kind {
            ResourceKind::Shell => push_new(&mut identity, canonical_shell_resource(resource)),
            ResourceKind::Path => push_new(&mut identity, canonical_path_resource(resource)),
            ResourceKind::Opaque => {}
        }
        let mut deny_only = Vec::new();
        for spelling in deny_only_spellings(kind, resource) {
            if !identity.contains(&spelling) {
                push_new(&mut deny_only, spelling);
            }
        }
        Self {
            kind,
            permission: permission.to_owned(),
            identity,
            deny_only,
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
        ResourceKind::Shell => push_new(&mut spellings, program_basename_resource(resource)),
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
/// One space between tokens, the program token unquoted and with a leading `\`
/// removed, and a leading `command` builtin dropped. Argument tokens keep their
/// own quoting, because a rule that mentions an argument means that argument as
/// the caller wrote it.
#[must_use]
pub fn canonical_shell_resource(command: &str) -> String {
    let tokens = shell_tokens(command);
    let tokens = without_command_builtin(&tokens);
    let Some((program, arguments)) = tokens.split_first() else {
        return command.trim().to_owned();
    };
    let mut canonical = program_spelling(program);
    for argument in arguments {
        canonical.push(' ');
        canonical.push_str(argument);
    }
    canonical
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

/// The same command line with the program reduced to its file name.
///
/// `/bin/rm -rf x` becomes `rm -rf x`. This drops which executable was named, so
/// only a `deny` rule is matched against it.
fn program_basename_resource(command: &str) -> String {
    let canonical = canonical_shell_resource(command);
    let tokens = shell_tokens(&canonical);
    let Some((program, arguments)) = tokens.split_first() else {
        return String::new();
    };
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .filter(|base| !base.is_empty() && *base != program);
    let Some(base) = base else {
        return String::new();
    };
    let mut reduced = base.to_owned();
    for argument in arguments {
        reduced.push(' ');
        reduced.push_str(argument);
    }
    reduced
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
fn without_command_builtin(tokens: &[String]) -> &[String] {
    let mut rest = tokens;
    while let Some((first, tail)) = rest.split_first() {
        if tail.is_empty() || program_spelling(first) != "command" {
            break;
        }
        rest = tail;
    }
    rest
}

/// The program a token names: surrounding quotes and a leading `\` removed.
///
/// Both spellings exist only to bypass the shell's alias and function lookup, so
/// they name the same program as the bare token.
fn program_spelling(token: &str) -> String {
    let mut program = token;
    loop {
        let unquoted = unquote(program);
        let unescaped = unquoted.strip_prefix('\\').unwrap_or(unquoted);
        if unescaped == program {
            return program.to_owned();
        }
        program = unescaped;
    }
}

fn unquote(token: &str) -> &str {
    for quote in ['\'', '"'] {
        if token.len() >= 2
            && token.starts_with(quote)
            && token.ends_with(quote)
            && let Some(inner) = token.get(1..token.len() - 1)
            && !inner.contains(quote)
        {
            return inner;
        }
    }
    token
}

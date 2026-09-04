//! Deterministic destructive-command assessment and its execution gate.
//!
//! This is **not a sandbox**. A command that passes this gate still has the
//! user's full filesystem, network, and credentials. The gate is a static,
//! side-effect-free tripwire for recognizable destructive operations; an actual
//! confinement layer remains a separate future decision.
//!
//! Assessment consumes [`crate::shell::analyze_command`]'s tree-sitter resources
//! instead of reparsing the command with a second shell tokenizer. That makes
//! every constituent and nested command visible while retaining the parser's
//! guarantee that assessment executes nothing.
//!
//! Confirmable operations require a fresh attached-user decision. Model-authored
//! arguments can describe the operation, but cannot authorize it. Catastrophic
//! targets are absolute denials.
//!
//! # Static-analysis boundary
//!
//! Empty brace alternatives are real expansions (`/{,}` includes `/`). Supported
//! alternatives are expanded recursively; unsupported or malformed brace syntax is
//! treated as an unknown target and reflected instead of assessed as a literal path.
//! The assessor also walks command resources in lexical order and conservatively
//! simulates `cd`, `chdir`, `pushd`/`popd`, and PowerShell location commands. A
//! dynamic location makes subsequent relative destructive targets unknown, so they
//! require confirmation. Conditional and nested-shell scope is deliberately
//! over-approximated: if a destructive constituent can run, it is assessed, even
//! when another constituent may prevent it at runtime.
//!
//! This gate recognizes destruction; it does not detect read-based exfiltration.
//! For example, `cat ~/.ssh/id_rsa` is intentionally allowed by this gate because it
//! does not destroy the credential. Permission policy or future confinement must
//! govern whether such a read may execute or transmit its output.
//!
//! Static analysis still cannot prove arbitrary interpreter or application
//! semantics, shell aliases or functions, encoded or downloaded scripts, or
//! time-of-check/time-of-use behavior. Static redirect targets are inspected only
//! to distinguish creation from replacement; that probe is not confinement.

use crate::shell::{CommandResource, ShellSyntax, analyze_command};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use zuno_error::ToolError;

const MAX_EMBEDDED_SCRIPT_DEPTH: usize = 4;

const DESTRUCTIVE_COMMANDS: &[&str] = &[
    "rm",
    "rmdir",
    "shred",
    "unlink",
    "truncate",
    "dd",
    "mkfs",
    "fdisk",
    "parted",
    "wipefs",
    "srm",
    "remove-item",
    "del",
    "erase",
    "rd",
    "format",
];

/// Programs that run the command that follows their own options. Each has an entry in
/// [`WRAPPER_OPTIONS`]; `su` is not one because it is read by [`assess_su_script`].
const WRAPPER_COMMANDS: &[&str] = &[
    "sudo", "doas", "env", "nice", "ionice", "time", "timeout", "nohup", "xargs", "command",
    "builtin", "exec", "setsid", "stdbuf", "chroot", "watch", "chrt", "taskset", "flock",
];

/// Shells whose `-c` script is followed into. Shared with [`crate::navigation`] so a
/// nested script is classified by what it runs under both gates, from one table.
pub(crate) const SHELL_COMMANDS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "pwsh",
    "powershell",
];

const CREDENTIAL_SUBPATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".docker",
    ".config/gh",
    ".netrc",
    ".password-store",
    ".local/share/keyrings",
    "Library/Keychains",
];

const PROTECTED_HOME_SUBPATHS: &[&str] = &[
    ".config",
    ".jcode",
    ".claude",
    ".local",
    ".local/share",
    "Documents",
    "Desktop",
];

const PROTECTED_SYSTEM_PATHS: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib64",
    "/opt",
    "/proc",
    "/root",
    "/sbin",
    "/srv",
    "/sys",
    "/usr",
    "/var",
    "/Applications",
    "/System",
    "/Library",
    "/Users",
    "/home",
];

const RECURSIVE_SYSTEM_PATHS: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/proc", "/sbin", "/sys", "/usr",
    "/var/lib", "/System", "/Library",
];

/// Windows locations that are catastrophic to remove exactly, drive-relative.
///
/// `users` is deliberately not recursive: `C:\Users\alice\project` is ordinary work,
/// exactly as `/home` and `/Users` are equality-only above.
const PROTECTED_WINDOWS_SUBPATHS: &[&str] = &["users", "users/public", "$recycle.bin"];

/// Windows locations that are catastrophic to remove at or below, drive-relative.
const RECURSIVE_WINDOWS_SUBPATHS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "boot",
    "perflogs",
    "system volume information",
];

/// Every spelling of "the user's home directory" that a Windows shell expands itself.
///
/// `cmd` uses `%USERPROFILE%` and PowerShell uses `$env:USERPROFILE`; neither sets
/// `HOME`. All entries are lowercase because Windows environment variable names are
/// case-insensitive and [`replace_ignoring_case`] folds the subject to match. The
/// braced spelling precedes the bare one so the longer match wins.
const WINDOWS_HOME_SPELLINGS: &[&str] =
    &["%userprofile%", "${env:userprofile}", "$env:userprofile"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Safe,
    Confirm,
    Catastrophic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskKind {
    Generic,
    GitHistoryRewrite,
    GitPublishedHistoryRewrite,
}

pub(crate) const GIT_REPOSITORY_ENVIRONMENT_VARIABLES: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_INDEX_FILE",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskFinding {
    pub level: RiskLevel,
    pub kind: RiskKind,
    pub reason: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskAssessment {
    pub level: RiskLevel,
    pub findings: Vec<RiskFinding>,
}

impl RiskAssessment {
    fn from_findings(findings: Vec<RiskFinding>) -> Self {
        let level = findings
            .iter()
            .map(|finding| finding.level)
            .max()
            .unwrap_or(RiskLevel::Safe);
        Self { level, findings }
    }

    #[must_use]
    pub fn explanation(&self) -> String {
        let mut explanation = String::new();
        for finding in &self.findings {
            explanation.push_str("- ");
            explanation.push_str(&finding.reason);
            if let Some(target) = &finding.target {
                explanation.push_str(" (target: ");
                explanation.push_str(target);
                explanation.push(')');
            }
            explanation.push('\n');
        }
        explanation
    }

    /// Whether the command rewrites local Git history and therefore must name
    /// the exact HEAD value observed before the Shell call.
    #[must_use]
    pub fn requires_expected_git_head(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.kind == RiskKind::GitHistoryRewrite)
    }
}

/// Environment facts needed for lexical path resolution.
///
/// Keeping these as values makes lexical resolution deterministic.
/// [`RiskContext::from_env`] snapshots the home directory once; redirect assessment
/// may still inspect one fully resolved static target to distinguish creation from
/// replacement.
#[derive(Debug, Clone, Default)]
pub struct RiskContext {
    pub working_dir: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
}

impl RiskContext {
    #[must_use]
    pub fn from_env(working_dir: Option<PathBuf>) -> Self {
        Self {
            working_dir,
            home_dir: home_directory_from(std::env::var_os("HOME"), std::env::home_dir()),
        }
    }
}

/// The home directory to protect, preferring an explicit `HOME` over the platform's.
///
/// `HOME` alone is not enough. Neither `cmd` nor PowerShell sets it, so on Windows
/// `home_dir` was `None`, and with it every home, credential, and profile rule below
/// silently switched off: `rm -rf ~/.ssh` and `rm -rf $HOME` fell from a permanent
/// refusal to a confirmation prompt, and executed outright under `allow_all`.
/// [`std::env::home_dir`] reads `USERPROFILE` there. `HOME` still takes precedence so
/// a user who deliberately relocates it keeps the behaviour they configured.
///
/// Taking both inputs as parameters keeps the decision testable without mutating
/// process environment shared with every other test in the binary.
fn home_directory_from(variable: Option<OsString>, platform: Option<PathBuf>) -> Option<PathBuf> {
    variable
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or(platform)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    Allow,
    Confirm {
        reason: String,
        target: Option<String>,
    },
    Deny {
        reason: String,
    },
}

pub fn assess_command(
    command: &str,
    syntax: ShellSyntax,
    context: &RiskContext,
) -> Result<RiskAssessment, ToolError> {
    assess_command_at_depth(command, syntax, context, 0)
}

fn assess_command_at_depth(
    command: &str,
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
) -> Result<RiskAssessment, ToolError> {
    let analysis = analyze_command(command, syntax)?;
    let mut findings = Vec::new();
    let mut location = SimulatedLocation::new(context.working_dir.clone());
    for resource in &analysis.commands {
        let resource_context = RiskContext {
            working_dir: location.cwd.clone(),
            home_dir: context.home_dir.clone(),
        };
        assess_resource(resource, syntax, &resource_context, depth, &mut findings)?;
        if resource.changes_directory {
            location.apply(resource, syntax, context.home_dir.as_deref());
        }
    }
    Ok(RiskAssessment::from_findings(findings))
}

#[derive(Debug, Clone)]
struct SimulatedLocation {
    cwd: Option<PathBuf>,
    stack: Vec<Option<PathBuf>>,
}

impl SimulatedLocation {
    fn new(cwd: Option<PathBuf>) -> Self {
        Self {
            cwd: cwd.map(|path| normalize_path(&path)),
            stack: Vec::new(),
        }
    }

    fn apply(&mut self, resource: &CommandResource, syntax: ShellSyntax, home: Option<&Path>) {
        let Some(program) = resource
            .tokens
            .first()
            .map(|token| command_name(token, syntax))
        else {
            self.cwd = None;
            return;
        };
        let context = RiskContext {
            working_dir: self.cwd.clone(),
            home_dir: home.map(Path::to_path_buf),
        };
        let source_has_unknown_expansion = is_dynamic_path(&resource.source)
            && !home_is_fully_expanded(&resource.source, &context);
        match program.as_str() {
            "cd" | "chdir" | "set-location" => {
                self.cwd = if source_has_unknown_expansion {
                    None
                } else {
                    directory_argument(&resource.tokens, &program, syntax)
                        .or_else(|| {
                            (resource.tokens.len() == 1 && program != "set-location")
                                .then(|| "~".to_owned())
                        })
                        .and_then(|target| resolve_directory_target(&target, &context, syntax))
                };
            }
            "pushd" => {
                let target = directory_argument(&resource.tokens, &program, syntax);
                match target {
                    Some(target) => {
                        self.stack.push(self.cwd.clone());
                        self.cwd = if source_has_unknown_expansion {
                            None
                        } else {
                            resolve_directory_target(&target, &context, syntax)
                        };
                    }
                    None => self.swap_with_stack_top(),
                }
            }
            "push-location" => {
                self.stack.push(self.cwd.clone());
                if let Some(target) = directory_argument(&resource.tokens, &program, syntax) {
                    self.cwd = if source_has_unknown_expansion {
                        None
                    } else {
                        resolve_directory_target(&target, &context, syntax)
                    };
                }
            }
            "popd" | "pop-location" => {
                self.cwd = if resource.tokens.len() == 1 {
                    self.stack.pop().flatten()
                } else {
                    None
                };
            }
            _ => self.cwd = None,
        }
    }

    fn swap_with_stack_top(&mut self) {
        let Some(previous) = self.stack.last_mut() else {
            self.cwd = None;
            return;
        };
        std::mem::swap(&mut self.cwd, previous);
    }
}

fn directory_argument(tokens: &[String], program: &str, syntax: ShellSyntax) -> Option<String> {
    let mut index = 1;
    while let Some(raw) = tokens.get(index) {
        let token = unquote(raw);
        if token == "--" {
            return tokens.get(index + 1).map(|target| unquote(target));
        }
        if syntax == ShellSyntax::PowerShell {
            let option = token.to_ascii_lowercase();
            if matches!(option.as_str(), "-path" | "-literalpath") {
                return tokens.get(index + 1).map(|target| unquote(target));
            }
            if matches!(option.as_str(), "-stackname" | "-passthru") {
                index += usize::from(option == "-stackname") + 1;
                continue;
            }
        }
        if program == "pushd" && (token == "-n" || is_directory_stack_index(&token)) {
            return None;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(token);
    }
    None
}

fn is_directory_stack_index(token: &str) -> bool {
    token
        .strip_prefix('+')
        .or_else(|| token.strip_prefix('-'))
        .is_some_and(|index| {
            !index.is_empty() && index.chars().all(|character| character.is_ascii_digit())
        })
}

fn resolve_directory_target(
    raw: &str,
    context: &RiskContext,
    syntax: ShellSyntax,
) -> Option<PathBuf> {
    if matches!(static_brace_expansions(raw), BraceExpansions::Unknown)
        || (is_dynamic_path(raw) && !home_is_fully_expanded(raw, context))
    {
        return None;
    }
    let expanded = expand_path(raw, context, syntax);
    is_rooted(&expanded).then(|| normalize_path(&expanded))
}

#[must_use]
pub fn gate(assessment: &RiskAssessment) -> GateOutcome {
    match assessment.level {
        RiskLevel::Safe => GateOutcome::Allow,
        RiskLevel::Catastrophic => GateOutcome::Deny {
            reason: format!(
                "This command is blocked and cannot be confirmed.\n\n{}\
                 If the user genuinely wants this, they must run it themselves outside the agent.",
                assessment.explanation()
            ),
        },
        RiskLevel::Confirm => GateOutcome::Confirm {
            reason: assessment.explanation().trim_end().to_owned(),
            target: assessment
                .findings
                .iter()
                .find_map(|finding| finding.target.clone()),
        },
    }
}

pub fn assess_and_gate(
    command: &str,
    syntax: ShellSyntax,
    context: &RiskContext,
) -> Result<(RiskAssessment, GateOutcome), ToolError> {
    let assessment = assess_command(command, syntax, context)?;
    let outcome = gate(&assessment);
    match &outcome {
        GateOutcome::Allow => tracing::info!(
            target: "zuno_tools::risk",
            verdict = "run",
            command_bytes = command.len(),
            syntax = ?syntax,
            "destructive-command gate verdict"
        ),
        GateOutcome::Confirm { .. } => tracing::info!(
            target: "zuno_tools::risk",
            verdict = "confirm",
            command_bytes = command.len(),
            syntax = ?syntax,
            "destructive-command gate verdict"
        ),
        GateOutcome::Deny { .. } => tracing::warn!(
            target: "zuno_tools::risk",
            verdict = "deny",
            command_bytes = command.len(),
            syntax = ?syntax,
            "destructive-command gate verdict"
        ),
    }
    Ok((assessment, outcome))
}

fn assess_resource(
    resource: &CommandResource,
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
    findings: &mut Vec<RiskFinding>,
) -> Result<(), ToolError> {
    for redirect in truncating_redirect_targets(&resource.source) {
        assess_redirect_target(&redirect, context, syntax, findings);
    }

    let tokens = &resource.tokens;
    // An `env` line whose every word is its own runs nothing at all. The script it hands
    // to a shell is *not* intercepted here: the walk below reads `env -S` wherever it
    // appears, including behind another wrapper, and reads every word that may be the
    // script rather than only the first.
    if env_without_child_command(tokens, syntax) {
        return Ok(());
    }

    // Every way the wrappers can be read is judged and the findings are the union: a
    // reading can add a confirmation or a denial, never take one away. Stopping at the
    // first reading made `env $FOO rm -rf /` a confirmation — the computed word ended
    // the assessment with `rm -rf /` still on the line — and `exec -a foo rm -rf /` an
    // `Allow`, because `-a` was read as a flag and `foo` was judged instead of `rm`.
    let (readings, saturated) = wrapper_walk(tokens, syntax);
    for reading in readings {
        assess_program(resource, reading, syntax, context, depth, findings)?;
    }
    if saturated {
        findings.push(unknown_target_finding(
            "the command line has more computed words than the gate reads through, so the \
             program it runs cannot be checked statically"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Why a program word that still reads like a command line is held.
const UNSPLIT_PROGRAM: &str = "program word is not a single command name, so the line could \
                               not be split reliably and cannot be checked before it runs";

/// Whether a materialised program word is really a command line: it carries a list
/// operator or a line break, or it has whitespace and its first segment names a program
/// the gate would judge on its own (`rm -rf`, `sh -c`, `sudo rm`). A path whose only
/// whitespace is inside a directory name (`C:\Program Files\…\git.exe`) is one name.
fn reads_like_a_command_line(word: &str, syntax: ShellSyntax) -> bool {
    if word
        .chars()
        .any(|character| matches!(character, ';' | '|' | '&' | '\n'))
    {
        return true;
    }
    let mut segments = word.split_whitespace();
    let Some(first) = segments.next() else {
        return false;
    };
    if segments.next().is_none() {
        return false;
    }
    // The whole word first: `"/opt/sh dir/rm" -rf /` is the program `rm` in a directory
    // whose name happens to start with `sh`, and it is judged as `rm` — held here it would
    // fall from a refusal to a prompt. Only a word that is not itself a judged program
    // is read by its first segment.
    if names_a_judged_program(&command_name(word, syntax)) {
        return false;
    }
    names_a_judged_program(&command_name(first, syntax))
}

/// Whether a program name is one the gate judges on its own rather than waves through.
fn names_a_judged_program(program: &str) -> bool {
    is_destructive_command(program)
        || SHELL_COMMANDS.contains(&program)
        || WRAPPER_COMMANDS.contains(&program)
        || matches!(program, "su" | "eval" | "find" | "git")
}

/// Hold a line whose words after a script still read like another command: the
/// tokenizer splits a single shell word such as `$'echo hi'\;$'rm -rf /'` into the
/// script `echo hi` and an "argument" `;rm -rf /`, and the shell runs both. `$0` and
/// positional arguments never contain a list operator or a line break on purpose.
fn hold_command_like_arguments(
    program: &str,
    arguments: &[String],
    syntax: ShellSyntax,
    findings: &mut Vec<RiskFinding>,
) {
    for argument in arguments {
        let materialised = static_shell_word(argument, syntax);
        if materialised
            .chars()
            .any(|character| matches!(character, ';' | '|' | '&' | '\n'))
        {
            findings.push(unknown_target_finding(format!(
                "`{program}` receives an argument that reads like another command, so the \
                 line could not be split reliably and cannot be checked statically"
            )));
            return;
        }
    }
}

/// Assess one reading of what a command line runs: the program at the front of the
/// reading with everything after it as its arguments, or the script a wrapper hands to
/// a shell.
fn assess_program(
    resource: &CommandResource,
    reading: WrapperReading<'_>,
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
    findings: &mut Vec<RiskFinding>,
) -> Result<(), ToolError> {
    let (tokens, wrapper) = match reading {
        WrapperReading::Script { script, runner } => {
            return assess_embedded_script(script, syntax, context, depth, runner, findings);
        }
        WrapperReading::Command { command, wrapper } => (command, wrapper),
    };
    let Some(first) = tokens.first() else {
        if let Some(wrapper) = wrapper {
            findings.push(unknown_target_finding(format!(
                "`{wrapper}` runs a command that could not be identified statically"
            )));
        }
        return Ok(());
    };
    // A wrapper's script option can carry the script in the option word itself
    // (`env -S'rm -rf /'`, `env --split-string='rm -rf /'`). The script is then text
    // inside one word rather than words of the line, so the walk hands out the reading
    // that starts at that word and the script is materialised here.
    if let Some(wrapper) = wrapper.as_deref()
        && let Some(option) = script_option(wrapper, &static_shell_word(first, syntax))
        && let Some(attached) = option.attached
    {
        let mut script = vec![attached];
        if option.takes_rest {
            // `env -S'rm -rf' /` splits the attached string and appends the rest.
            script.extend_from_slice(&tokens[1..]);
        }
        return assess_embedded_script(&script, syntax, context, depth, option.runner, findings);
    }
    // The program a wrapper runs gets the same reading as a bare one: `sudo rm$'' -rf /`
    // and `env rm${IFS}-rf${IFS}/` were checked only as `sudo` and `env`.
    if is_dynamic_word(first, syntax) {
        findings.push(unknown_target_finding(match &wrapper {
            Some(wrapper) => format!("`{wrapper}` runs a command whose {DYNAMIC_PROGRAM}"),
            None => DYNAMIC_PROGRAM.to_owned(),
        }));
        return Ok(());
    }
    // A materialised word that still reads like a command line is not one program name
    // the gate can judge: `$'rm -rf' /` materialises to `rm -rf`, and the tokenizer may
    // split a single shell word (`$'echo hi'\;$'rm -rf /'` arrives as two tokens while
    // bash runs `rm -rf /`). Such a word is held for a human. A quoted program path with
    // a space in it (`"C:\Program Files\Git\cmd\git.exe" --version`) is one name, not a
    // command line, and is judged like any other program.
    let materialised = static_shell_word(first, syntax);
    if reads_like_a_command_line(&materialised, syntax) {
        findings.push(unknown_target_finding(match &wrapper {
            Some(wrapper) => format!("`{wrapper}` runs a command whose {UNSPLIT_PROGRAM}"),
            None => UNSPLIT_PROGRAM.to_owned(),
        }));
        return Ok(());
    }
    let program = command_name(first, syntax);

    if program == "eval" {
        return assess_embedded_script(&tokens[1..], syntax, context, depth, "eval", findings);
    }
    if SHELL_COMMANDS.contains(&program.as_str()) {
        return assess_shell_script(tokens, syntax, context, depth, &program, findings);
    }
    if program == "su" {
        return assess_su_script(tokens, syntax, context, depth, findings);
    }
    if program == "find" {
        return assess_find(tokens, syntax, context, depth, findings);
    }
    if program == "git" && assess_git(&resource.source, tokens, syntax, context, findings) {
        return Ok(());
    }

    if !is_destructive_command(&program) {
        return Ok(());
    }

    let mut targets = destructive_targets(tokens, &program, syntax);
    if targets.is_empty() && source_mentions_home_target(&resource.source) {
        targets.push("$HOME".to_owned());
    }
    if targets.is_empty() {
        findings.push(unknown_target_finding(format!(
            "`{program}` is destructive but its target could not be determined statically"
        )));
        return Ok(());
    }
    let absent_temp_file_cleanup = is_forced_non_recursive_rm(&program, tokens);
    for target in targets {
        assess_destructive_target(&target, context, syntax, absent_temp_file_cleanup, findings);
    }
    Ok(())
}

fn assess_git(
    source: &str,
    tokens: &[String],
    syntax: ShellSyntax,
    context: &RiskContext,
    findings: &mut Vec<RiskFinding>,
) -> bool {
    let Some((subcommand, args)) = git_subcommand(tokens, syntax) else {
        return false;
    };
    if is_dynamic_literal(&subcommand, syntax) {
        findings.push(unknown_target_finding(format!(
            "`git` runs a subcommand whose {DYNAMIC_PROGRAM}"
        )));
        // The expansion may vanish or be a global option, so the call is read again
        // without it: `git $EMPTY push --force` force-pushes when `EMPTY` is unset, and
        // stopping here made that a confirmation.
        let mut without = tokens[..tokens.len() - args.len() - 1].to_vec();
        without.extend_from_slice(args);
        assess_git(source, &without, syntax, context, findings);
        return true;
    }
    let repository_override =
        git_uses_repository_override(tokens) || source_uses_git_repository_environment(source);
    match subcommand.as_str() {
        "clean" => {
            let target = context
                .working_dir
                .as_deref()
                .map_or_else(|| ".".to_owned(), |cwd| cwd.display().to_string());
            findings.push(confirm_finding(
                "`git clean` irreversibly removes untracked files".to_owned(),
                Some(target),
            ));
            true
        }
        "commit" if has_git_option(args, syntax, "--amend", None) => {
            assess_local_git_history_rewrite(
                repository_override,
                "`git commit --amend` replaces the current commit",
                findings,
            );
            true
        }
        "rebase" if !is_rebase_recovery(args, syntax) => {
            assess_local_git_history_rewrite(
                repository_override,
                "`git rebase` rewrites local commit history",
                findings,
            );
            true
        }
        "tag" if has_git_option(args, syntax, "--force", Some('f')) => {
            assess_local_git_history_rewrite(
                repository_override,
                "`git tag --force` moves an existing tag",
                findings,
            );
            true
        }
        "push" => assess_git_push(args, syntax, findings),
        _ => false,
    }
}

fn source_uses_git_repository_environment(source: &str) -> bool {
    let source = source.to_ascii_uppercase();
    GIT_REPOSITORY_ENVIRONMENT_VARIABLES
        .iter()
        .any(|variable| source.contains(&format!("{variable}=")))
}

fn assess_local_git_history_rewrite(
    repository_override: bool,
    reason: &str,
    findings: &mut Vec<RiskFinding>,
) {
    if repository_override {
        findings.push(RiskFinding {
            level: RiskLevel::Catastrophic,
            kind: RiskKind::Generic,
            reason: format!(
                "{reason}; repository-changing Git global options are refused because \
                 `expectedGitHead` is bound to the Shell workdir; select the repository with \
                 `workdir` instead"
            ),
            target: None,
        });
    } else {
        findings.push(git_history_finding(reason.to_owned()));
    }
}

/// Whether a git invocation chooses its repository with a global option.
///
/// `-C`, `--git-dir`, `--work-tree` and `--namespace`, in both the separated and the
/// `=` spelling. Shared with [`crate::shell`], which refuses a commit that retargets
/// the repository rather than inspecting a different one than the commit writes.
pub(crate) fn git_uses_repository_override(tokens: &[String]) -> bool {
    let mut index = 1;
    while let Some(raw) = tokens.get(index) {
        let token = unquote(raw);
        if token == "--" || !token.starts_with('-') || token == "-" {
            return false;
        }
        if matches!(
            token.as_str(),
            "-C" | "--git-dir" | "--work-tree" | "--namespace"
        ) || token.starts_with("-C")
            || token.starts_with("--git-dir=")
            || token.starts_with("--work-tree=")
            || token.starts_with("--namespace=")
        {
            return true;
        }
        let consumes_value = matches!(token.as_str(), "-c" | "--config-env" | "--exec-path");
        index += 1 + usize::from(consumes_value && !token.contains('='));
    }
    false
}

/// The subcommand of a git invocation, lowercased, and the arguments after it.
///
/// `None` when the command is not git or names no subcommand. The program is reduced by
/// [`command_name`], so `/usr/bin/git`, `GIT` and `git.exe` are all git: spelling the
/// comparison out here instead let a path-qualified or `.exe`-suffixed git walk past
/// every check keyed on a subcommand. The subcommand and the global options before it
/// get the same per-character reading ([`static_shell_word`]): `p''ush`, `'push'` and
/// `p"ush"` are all `push` to the shell, and [`unquote`] — which strips one matched
/// outer pair and nothing else — let `git p''ush --force` reach no check at all.
/// Global options are skipped the way git parses them, so the subcommand is the first
/// token that is not one. A subcommand the shell has to compute (`git $SUB`) comes back
/// with its `$`, so the caller can see it is dynamic.
pub(crate) fn git_subcommand(
    tokens: &[String],
    syntax: ShellSyntax,
) -> Option<(String, &[String])> {
    if tokens
        .first()
        .is_none_or(|token| command_name(token, syntax) != "git")
    {
        return None;
    }
    let mut index = 1;
    while let Some(raw) = tokens.get(index) {
        let token = static_shell_word(raw, syntax);
        if token == "--" {
            index += 1;
            break;
        }
        if !token.starts_with('-') || token == "-" {
            return Some((token.to_ascii_lowercase(), &tokens[index + 1..]));
        }
        let consumes_value = matches!(
            token.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix"
        );
        index += 1 + usize::from(consumes_value && !token.contains('='));
    }
    tokens.get(index).map(|token| {
        (
            static_shell_word(token, syntax).to_ascii_lowercase(),
            &tokens[index + 1..],
        )
    })
}

/// Whether `args` carries `long` (or `-x…` with `short` among the flags), read the way
/// the shell reads each word so `--for''ce` is `--force`.
fn has_git_option(args: &[String], syntax: ShellSyntax, long: &str, short: Option<char>) -> bool {
    args.iter()
        .map(|token| static_shell_word(token, syntax))
        .any(|token| {
            token == long
                || token.starts_with(&format!("{long}="))
                || short.is_some_and(|short| {
                    token
                        .strip_prefix('-')
                        .filter(|flags| !flags.starts_with('-'))
                        .is_some_and(|flags| flags.contains(short))
                })
        })
}

fn is_rebase_recovery(args: &[String], syntax: ShellSyntax) -> bool {
    [
        "--abort",
        "--continue",
        "--skip",
        "--quit",
        "--show-current-patch",
    ]
    .iter()
    .any(|option| has_git_option(args, syntax, option, None))
}

fn assess_git_push(args: &[String], syntax: ShellSyntax, findings: &mut Vec<RiskFinding>) -> bool {
    let rendered = args
        .iter()
        .map(|token| static_shell_word(token, syntax))
        .collect::<Vec<_>>();
    let unsafe_force = rendered.iter().any(|token| {
        token == "--force"
            || token == "-f"
            || token
                .strip_prefix('-')
                .filter(|flags| !flags.starts_with('-'))
                .is_some_and(|flags| flags.contains('f'))
            || (!token.starts_with('-') && token.starts_with('+'))
    });
    let leases = rendered
        .iter()
        .filter(|token| token.starts_with("--force-with-lease"))
        .collect::<Vec<_>>();
    let valid_explicit_lease = |token: &&String| {
        token
            .strip_prefix("--force-with-lease=")
            .and_then(|value| value.split_once(':'))
            .is_some_and(|(reference, expected)| !reference.is_empty() && exact_git_oid(expected))
    };
    let explicit_lease = leases.iter().any(valid_explicit_lease);
    let malformed_lease = leases.iter().any(|token| !valid_explicit_lease(token));
    if unsafe_force || malformed_lease {
        findings.push(RiskFinding {
            level: RiskLevel::Catastrophic,
            kind: RiskKind::GitPublishedHistoryRewrite,
            reason: "published Git history may only be rewritten with an explicit atomic lease \
                     such as `--force-with-lease=refs/heads/main:<expected-full-oid>`"
                .to_owned(),
            target: None,
        });
        return true;
    }
    if explicit_lease {
        findings.push(RiskFinding {
            level: RiskLevel::Confirm,
            kind: RiskKind::GitPublishedHistoryRewrite,
            reason: "`git push --force-with-lease` rewrites published history after checking the \
                     caller-supplied remote object id"
                .to_owned(),
            target: None,
        });
        return true;
    }
    false
}

fn exact_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Where an `env` invocation's own words stop.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvReading {
    /// `env -S SCRIPT`, in any of its spellings, and the words after it.
    Script(Vec<String>),
    /// Every word is `env`'s own, so nothing runs: `env`, `env -u X`, `env FOO=bar`.
    NoChildCommand,
    /// A child command follows.
    ChildCommand,
}

/// How far `env`'s own options and assignments reach, and what follows them.
///
/// One scan answers both questions, so a word [`script_option`] can claim can never be
/// read as an ordinary value here and as a script there: `env -iS 'rm -rf /'` was an
/// environment query with no child command, which ends assessment, while the same line
/// hands `rm -rf /` to a shell.
fn env_reading(tokens: &[String], syntax: ShellSyntax) -> Option<EnvReading> {
    if tokens
        .first()
        .is_none_or(|token| command_name(token, syntax) != "env")
    {
        return None;
    }
    let mut index = 1;
    // `--` ends the options only. GNU `env` still reads the assignments after it, so
    // `env -u SECRET -- SAFE=1` sets a variable and runs nothing (measured).
    let mut options = true;
    while let Some(token) = tokens.get(index) {
        let word = static_shell_word(token, syntax);
        if options && word == "--" {
            options = false;
            index += 1;
            continue;
        }
        if options && let Some(option) = script_option("env", &word) {
            let (mut script, rest) = match option.attached {
                Some(attached) => (vec![attached], index + 1),
                // The option is there but its script is not, so `env` runs something this
                // scan cannot name; the walk reports that rather than reporting no child.
                None => match tokens.get(index + 1) {
                    Some(script) => (vec![script.clone()], index + 2),
                    None => return Some(EnvReading::ChildCommand),
                },
            };
            script.extend_from_slice(&tokens[rest.min(tokens.len())..]);
            return Some(EnvReading::Script(script));
        }
        if options && word.starts_with('-') {
            index += 1;
            if wrapper_option_arity("env", &word) == OptionArity::Value
                && tokens.get(index).is_some()
            {
                index += 1;
            }
            continue;
        }
        if word.contains('=') {
            index += 1;
            continue;
        }
        return Some(EnvReading::ChildCommand);
    }
    Some(EnvReading::NoChildCommand)
}

fn env_without_child_command(tokens: &[String], syntax: ShellSyntax) -> bool {
    env_reading(tokens, syntax) == Some(EnvReading::NoChildCommand)
}

fn env_split_script(tokens: &[String], syntax: ShellSyntax) -> Option<Vec<String>> {
    match env_reading(tokens, syntax) {
        Some(EnvReading::Script(script)) => Some(script),
        _ => None,
    }
}

fn assess_shell_script(
    tokens: &[String],
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
    program: &str,
    findings: &mut Vec<RiskFinding>,
) -> Result<(), ToolError> {
    let Some(option) = tokens
        .iter()
        .position(|token| is_command_script_option(&static_shell_word(token, syntax)))
    else {
        return Ok(());
    };
    let operands = &tokens[option + 1..];
    let offsets = script_operand_offsets(operands, syntax);
    for offset in &offsets {
        assess_embedded_script(
            std::slice::from_ref(&operands[*offset]),
            syntax,
            context,
            depth,
            program,
            findings,
        )?;
    }
    if let Some(last) = offsets.last() {
        hold_command_like_arguments(program, &operands[*last + 1..], syntax, findings);
    }
    Ok(())
}

/// The positions in `operands` of every word that may be an inline script, in order.
///
/// Further options come first — `sh -c -x 'rm -rf /'` runs `rm -rf /` under `-x` — and
/// a word the shell computes may vanish or be an option itself — `sh -c $EMPTY 'rm -rf /'`
/// runs `rm -rf /` when `EMPTY` is unset — so every computed word and then the first
/// static operand are all read as the script. Taking the word right after `-c` made the
/// first an `Allow` and the second a confirmation. Shared with the wrapper walk, so
/// `env -S $EMPTY 'rm -rf /'` and `flock … -c $EMPTY 'rm -rf /'` are read the same way.
fn script_operand_offsets(operands: &[String], syntax: ShellSyntax) -> Vec<usize> {
    let mut scripts = Vec::new();
    for (offset, token) in operands.iter().enumerate() {
        let word = static_shell_word(token, syntax);
        if is_dynamic_literal(&word, syntax) {
            scripts.push(offset);
        } else if !word.starts_with('-') {
            scripts.push(offset);
            break;
        }
    }
    scripts
}

fn assess_su_script(
    tokens: &[String],
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
    findings: &mut Vec<RiskFinding>,
) -> Result<(), ToolError> {
    let mut scripts: Vec<Vec<String>> = Vec::new();
    if let Some(option) = tokens
        .iter()
        .position(|token| is_su_command_option(&static_shell_word(token, syntax)))
    {
        // `su --command='rm -rf /'` and `su -lc'rm -rf /'` carry the script in the option
        // word; the words after it are read as well, so a cluster whose letters only look
        // like `-c` cannot hide the script that follows.
        if let Some(attached) = attached_command_script(&static_shell_word(&tokens[option], syntax))
        {
            scripts.push(vec![attached]);
        }
        let operands = &tokens[option + 1..];
        let offsets = script_operand_offsets(operands, syntax);
        scripts.extend(offsets.iter().map(|offset| vec![operands[*offset].clone()]));
        if let Some(last) = offsets.last() {
            hold_command_like_arguments("su", &operands[*last + 1..], syntax, findings);
        }
    }
    if scripts.is_empty() {
        findings.push(unknown_target_finding(
            "`su` changes identity and its executed command could not be checked statically"
                .to_owned(),
        ));
        return Ok(());
    }
    for script in scripts {
        assess_embedded_script(&script, syntax, context, depth, "su", findings)?;
    }
    Ok(())
}

/// Whether `option` introduces an inline script for one of [`SHELL_COMMANDS`].
pub(crate) fn is_command_script_option(option: &str) -> bool {
    option.eq_ignore_ascii_case("-command")
        || option.eq_ignore_ascii_case("--command")
        || option
            .strip_prefix('-')
            .is_some_and(|flags| !flags.starts_with('-') && flags.contains('c'))
}

fn assess_embedded_script(
    tokens: &[String],
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
    runner: &str,
    findings: &mut Vec<RiskFinding>,
) -> Result<(), ToolError> {
    // The script is read the way the shell hands it to the nested interpreter — every
    // quote and escape removed — and then parsed again. Stripping one outer quote pair
    // instead left `'rm -rf '"/"` as a single word that named no program, and left a
    // `$'rm -rf /'` script as a word whose `$` made it merely unknown.
    let script = tokens
        .iter()
        .map(|token| static_shell_word(token, syntax))
        .collect::<Vec<_>>()
        .join(" ");
    let unknown = || {
        unknown_target_finding(format!(
            "`{runner}` runs a command whose destructive target cannot be checked statically"
        ))
    };
    if script.is_empty() || depth >= MAX_EMBEDDED_SCRIPT_DEPTH {
        findings.push(unknown());
        return Ok(());
    }
    // A script the shell computes part of is still read. Abandoning it at the first `$`
    // reported the uncertainty and nothing else, so one expansion the script never used
    // turned a permanent denial into a prompt: `sh -c 'rm -rf /'` was refused and
    // `sh -c 'rm -rf / $UNUSED'` was confirmable. The finding stays — the computed part
    // may still run anything — and the nested pass judges the static words beside it,
    // reporting a computed target as unknown on its own, exactly as [`assess_program`]
    // does for a computed program word.
    // The same holds for a script whose quoting does not close. The nested parser reads
    // the rest of it as one string, so a command inside it is invisible: materialising
    // `$'echo it\'s fine\nrm -rf /'` — an ANSI-C escape can spell a quote — left
    // `rm -rf /` inside an unterminated single-quoted word and the line reported nothing
    // at all. What cannot be read is reported as unread.
    if is_dynamic_path(&script) || has_unclosed_quote(&script) {
        findings.push(unknown());
    }
    let embedded = assess_command_at_depth(&script, syntax, context, depth + 1)?;
    findings.extend(embedded.findings);
    Ok(())
}

/// Whether a quote in `script` is still open at its end, so a parser reads the rest of
/// the text as one string instead of as commands.
fn has_unclosed_quote(script: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for character in script.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                }
            }
            Some(_) => match character {
                '\\' => escaped = true,
                '"' => quote = None,
                _ => {}
            },
            None => match character {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(character),
                _ => {}
            },
        }
    }
    quote.is_some()
}

/// Strip `sudo`, `env VAR=x`, `timeout 5`, `xargs` and the other wrappers from the
/// front of a command, returning what they run and the innermost wrapper's name.
///
/// This is the primary reading of [`wrapper_readings`] — an option the tables do not
/// know is read as a flag, a computed word as the program. The risk gate judges every
/// reading; the navigation and permission layers want one command line and take this
/// one.
///
/// Shared with [`crate::navigation`] so `env FOO=1 rg x` is `rg` under both gates;
/// two wrapper tables would drift, and a wrapper only one of them knew would let a
/// command through the other.
pub(crate) fn unwrap_wrappers(
    tokens: &[String],
    syntax: ShellSyntax,
) -> (&[String], Option<String>) {
    match wrapper_readings(tokens, syntax).into_iter().next() {
        Some(WrapperReading::Command { command, wrapper }) => (command, wrapper),
        Some(WrapperReading::Script { script, runner }) => (script, Some(runner.to_owned())),
        None => (tokens, None),
    }
}

/// One way to read what a command line runs once its wrappers are stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WrapperReading<'a> {
    /// A program and its arguments; `command` is empty when the wrappers ran out of
    /// words.
    Command {
        command: &'a [String],
        /// The innermost wrapper that runs `command`, if there was one.
        wrapper: Option<String>,
    },
    /// Words a wrapper hands to a shell as a script rather than running as a program:
    /// `env -S SCRIPT…`, `flock FILE -c SCRIPT`.
    Script {
        script: &'a [String],
        runner: &'static str,
    },
}

/// Every way the words of a command line can be read once its wrappers are stripped,
/// primary reading first.
///
/// A wrapper table cannot be complete and a computed word cannot be placed at all, so
/// the walk does not silently pick one word as the program. An option the tables do
/// not know forks the reading: it may be a flag, or the word after it may be its
/// value. A word the shell computes may vanish or expand to any number of the
/// wrapper's options, so every later word may be where the program starts; in the
/// value slot of an option the tables know, it may vanish and leave the next word as
/// the value, so every word after that may be the program. The primary reading —
/// unknown options as flags, computed words as programs or values — comes first; the
/// rest can only add findings.
///
/// `exec -a foo rm -rf /` had one reading, the program `foo`: `-a` was not in the
/// table, so `foo` was judged and found harmless and `rm` was never examined. With the
/// walk, an unknown option costs at most a confirmation, never a denial.
pub(crate) fn wrapper_readings(tokens: &[String], syntax: ShellSyntax) -> Vec<WrapperReading<'_>> {
    wrapper_walk(tokens, syntax).0
}

/// Every reading plus whether the walk stopped forking because the line had more than
/// [`MAX_WALK_STATES`] computed words; a saturated walk is incomplete and the
/// caller must hold the line.
fn wrapper_walk(tokens: &[String], syntax: ShellSyntax) -> (Vec<WrapperReading<'_>>, bool) {
    let mut walk = WrapperWalk {
        tokens,
        syntax,
        pending: vec![WalkState::Program {
            index: 0,
            wrapper: None,
        }],
        seen: HashSet::new(),
        produced: HashSet::new(),
        readings: Vec::new(),
        saturated: false,
        forks_dropped: false,
        script_readings: 0,
    };
    while let Some(state) = walk.pending.pop() {
        if walk.seen.len() >= MAX_WALK_STATES {
            walk.saturated = true;
            break;
        }
        if walk.seen.insert(state.clone()) {
            walk.follow(state);
        }
    }
    (walk.readings, walk.saturated || walk.forks_dropped)
}

/// Where a reading of the wrapper chain currently is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum WalkState {
    /// `index` is where a program is expected; `wrapper` is the innermost wrapper so
    /// far.
    Program {
        index: usize,
        wrapper: Option<String>,
    },
    /// `index` is among `wrapper`'s own options and operands. `done` records a `--`;
    /// `operands` counts the operands the wrapper still takes before its program.
    Options {
        index: usize,
        wrapper: String,
        done: bool,
        operands: u8,
    },
    /// `index` is where the script `wrapper` hands to a shell may start, because a
    /// computed word before it may have expanded to `wrapper`'s script option:
    /// `flock FILE $X-c $X 'rm -rf /'` runs `rm -rf /` when `X` is unset.
    Script { index: usize, wrapper: String },
}

/// The kind of a produced reading, so two paths that end at the same word add it once.
///
/// The wrapper is part of the key because the same word means different things to
/// different wrappers: in `sudo $X env -S'rm -rf /'` the word `-S'rm -rf /'` is a program
/// name to `sudo` and the script to `env`, and keying on the position alone dropped the
/// second reading — whichever path ran first won — so the line was only a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ReadingKind {
    Bare,
    Wrapped(String),
    Script(&'static str),
}

struct WrapperWalk<'a> {
    tokens: &'a [String],
    syntax: ShellSyntax,
    /// Readings still to follow; forks are pushed here and followed after the current
    /// path has produced its reading, so the primary reading is produced first.
    pending: Vec<WalkState>,
    seen: HashSet<WalkState>,
    produced: HashSet<(usize, ReadingKind)>,
    readings: Vec<WrapperReading<'a>>,
    /// Whether [`MAX_WALK_STATES`] readings were produced and later ones were dropped.
    saturated: bool,
    /// Whether a fork was dropped because the queue was full or
    /// [`MAX_SCRIPT_READINGS`] script readings already existed; the readings that were
    /// produced are still complete for the paths they follow.
    forks_dropped: bool,
    /// Speculative script readings produced so far; each one is a nested parse and walk.
    script_readings: usize,
}

/// Script readings one walk may produce. Every script reading re-parses the rest of the
/// line and walks it again, so the script readings, not the states, are where a long
/// line's work compounds: a thousand computed words after `env` cost tens of seconds
/// through them alone. Two hundred and fifty-six covers every real line by two orders of
/// magnitude.
const MAX_SCRIPT_READINGS: usize = 256;

/// Distinct walk states followed before the walk stops and the line is held.
///
/// Every computed word forks a reading at every later word and a later computed word
/// forks again, so a line of hundreds of computed words is cubic work: four hundred
/// took seconds and three thousand did not finish. A command a model composes on
/// purpose has a few dozen words and a handful of computed ones, a few hundred states
/// at most; the cap is an order of magnitude above that and turns the pathological
/// line into a prompt in well under a second instead of a stalled turn.
const MAX_WALK_STATES: usize = 4096;

impl<'a> WrapperWalk<'a> {
    /// Whether the walk has produced [`MAX_WALK_STATES`] readings; every reading is
    /// assessed, so the readings are bounded like the queue. Records saturation so the
    /// caller holds the line.
    fn saturate_if_full(&mut self) -> bool {
        if self.saturated || self.readings.len() >= MAX_WALK_STATES {
            self.saturated = true;
            return true;
        }
        false
    }

    fn command(&mut self, index: usize, wrapper: Option<String>) {
        if self.saturate_if_full() {
            return;
        }
        let kind = match &wrapper {
            Some(wrapper) => ReadingKind::Wrapped(wrapper.clone()),
            None => ReadingKind::Bare,
        };
        if self.produced.insert((index, kind)) {
            self.readings.push(WrapperReading::Command {
                command: &self.tokens[index..],
                wrapper,
            });
        }
    }

    /// Records a script reading. A *speculative* reading comes from a computed word that
    /// might have been the wrapper's script option; those are capped, because the walk
    /// pops them from the tail of a long line and each is a nested parse. A reading for
    /// an option that is really on the line (`env -S`, `flock -c`) is never dropped:
    /// `env $A -S 'rm -rf /' f0 … f300` is refused however long its tail.
    fn script(&mut self, start: usize, end: usize, runner: &'static str, speculative: bool) {
        if self.saturate_if_full() {
            return;
        }
        if speculative && self.script_readings >= MAX_SCRIPT_READINGS {
            self.forks_dropped = true;
            return;
        }
        if self.produced.insert((start, ReadingKind::Script(runner))) {
            if speculative {
                self.script_readings += 1;
            }
            self.readings.push(WrapperReading::Script {
                script: &self.tokens[start..end],
                runner,
            });
        }
    }

    /// Advances one reading past the word at `index`, which `wrapper` has just read as
    /// one of its own words, and queues `next` as the continuation of that reading.
    ///
    /// **This is the only place the walk consumes a word, so it is the only place the
    /// fork rule lives.** A word the shell computes — `$VAR`, `${VAR}`, `$(…)`, a
    /// backtick, a glob, and equally a computed part attached to a static one
    /// (`-u$EMPTY`, `-Eu$EMPTY`, `--user=$VAL`, `-n$N`) — may be empty at runtime. An
    /// empty unquoted word vanishes from the line entirely, and an empty attached value
    /// turns the attached spelling into the separated one, which takes the *next* word
    /// as the value. Both shift the program one or more words to the right:
    /// `sudo -u$EMPTY root rm -rf /` reaches `sudo` as `-u root rm -rf /` and runs
    /// `rm -rf /` as root. So wherever a computed word is consumed, for whatever reason,
    /// every later word is read as a place the program may start.
    ///
    /// Rounds three and four applied that rule at the program position and then at the
    /// separated value slot, each in its own arm; the same hole stayed open in the
    /// attached spelling and in the operand slot (`chroot $ROOT /mnt rm -rf /`) because
    /// each slot decided for itself. Now no slot decides.
    ///
    /// Every later word is read as a *program*, not as more of `wrapper`'s options:
    /// re-reading `-la` in `sudo -u $USER ls -la` as options of `sudo` would fork on the
    /// unknown `-a` and trade that `Allow` for a prompt, while as a program name `-la` is
    /// harmless. The one exception is a word that hands the wrapper a script
    /// (`env -u $EMPTY X -S 'rm -rf /'`, `flock -w $W 5 /tmp/lock -c 'rm -rf /'`): read
    /// as a program it is only an option name, so it is read as that option and the
    /// script is judged.
    ///
    /// This over-approximates on purpose. Words that are in fact arguments of an earlier
    /// one are read as programs too: `sudo $X echo rm -rf /` only prints, and the line
    /// is denied anyway. Guessing where an expansion ends would have to be right about a
    /// value the gate cannot see, and a wrong guess hides the program. A denial for a
    /// line that prints `rm -rf /` costs the user a rewording; a missed `rm -rf /` costs
    /// the filesystem. Fail closed.
    fn consume(&mut self, index: usize, wrapper: Option<&str>, next: Option<WalkState>) {
        if self
            .tokens
            .get(index)
            .is_some_and(|token| is_dynamic_word(token, self.syntax))
        {
            // The queue itself is bounded, not only the states followed: one computed
            // word queues two states per later word, so a few hundred computed words
            // would fill the queue by the million before the loop noticed. A full queue
            // stops forking and nothing else: the reading being followed still reaches
            // its program, so `env V0=$X0 … V61=$X61 rm -rf /` is refused, not prompted.
            if self.forks_dropped || self.pending.len() >= MAX_WALK_STATES {
                self.forks_dropped = true;
                self.pending.extend(next);
                return;
            }
            for later in index + 1..self.tokens.len() {
                let word = static_shell_word(&self.tokens[later], self.syntax);
                let hands_over_a_script =
                    wrapper.is_some_and(|wrapper| script_option(wrapper, &word).is_some());
                self.pending.push(match wrapper {
                    Some(wrapper) if hands_over_a_script => WalkState::Options {
                        index: later,
                        wrapper: wrapper.to_owned(),
                        done: false,
                        operands: 0,
                    },
                    wrapper => WalkState::Program {
                        index: later,
                        wrapper: wrapper.map(str::to_owned),
                    },
                });
                // The computed word may also expand to the wrapper's own script option,
                // and then a later word is a script rather than a program:
                // `flock FILE $X-c $X 'rm -rf /'` and `env $X-S $X 'rm -rf /'` both run
                // `rm -rf /` with `X` unset.
                if let Some(wrapper) = wrapper.filter(|wrapper| script_runner(wrapper).is_some()) {
                    self.pending.push(WalkState::Script {
                        index: later,
                        wrapper: wrapper.to_owned(),
                    });
                }
            }
        }
        // Queued last, so the reading that consumed nothing computed is followed — and
        // produces its reading — before any fork it just queued.
        self.pending.extend(next);
    }

    /// The word at `index` is `wrapper`'s inline-script option. The script is either
    /// carried in that word or given by the words after it.
    fn follow_script_option(&mut self, index: usize, wrapper: &str, option: &ScriptOption) {
        let script = index + 1;
        match &option.attached {
            Some(attached) => {
                // The script is text inside this word rather than words of the line, so
                // the reading is the word onward and [`assess_program`] materialises it.
                self.command(index, Some(wrapper.to_owned()));
                if is_dynamic_literal(attached, self.syntax) {
                    // The attached part may vanish, and then the option takes the next
                    // word as its script: `flock … -c$X 'rm -rf /'` runs `rm -rf /`.
                    self.script_candidates(script, option, false);
                }
            }
            None if script >= self.tokens.len() => {
                // The option with no script runs nothing that can be identified.
                return self.command(script, Some(wrapper.to_owned()));
            }
            None => self.script_candidates(script, option, false),
        }
        self.consume(index, Some(wrapper), None);
    }

    /// Every word from `start` on that may be the script, in order: the word right after
    /// the option, then — because a computed word may vanish or be another option — each
    /// computed word and the first static operand after it.
    fn script_candidates(&mut self, start: usize, option: &ScriptOption, speculative: bool) {
        let mut candidates = vec![0];
        candidates.extend(script_operand_offsets(&self.tokens[start..], self.syntax));
        for offset in candidates {
            let at = start + offset;
            let end = if option.takes_rest {
                self.tokens.len()
            } else {
                at + 1
            };
            self.script(at, end, option.runner, speculative);
        }
    }

    /// Reads the word one reading is currently at and queues how that reading continues.
    ///
    /// Every advance goes through [`WrapperWalk::consume`]; this function decides only
    /// *what* the word is, never whether a later word may be the program.
    fn follow(&mut self, state: WalkState) {
        match state {
            WalkState::Program { index, wrapper } => {
                let Some(token) = self.tokens.get(index) else {
                    return self.command(index, wrapper);
                };
                if is_dynamic_word(token, self.syntax) {
                    // The program a wrapper runs is consumed like any other computed
                    // word, and the reading at `index` is produced as well so the
                    // computed word itself is reported.
                    self.consume(index, wrapper.as_deref(), None);
                    return self.command(index, wrapper);
                }
                let program = command_name(token, self.syntax);
                if !WRAPPER_COMMANDS.contains(&program.as_str()) {
                    return self.command(index, wrapper);
                }
                let next = WalkState::Options {
                    index: index + 1,
                    operands: wrapper_operands(&program),
                    wrapper: program.clone(),
                    done: false,
                };
                self.consume(index, Some(&program), Some(next));
            }
            WalkState::Options {
                index,
                wrapper,
                done,
                operands,
            } => {
                let Some(token) = self.tokens.get(index) else {
                    return self.command(index, Some(wrapper));
                };
                let word = static_shell_word(token, self.syntax);
                let next = index + 1;
                if !done && word == "--" {
                    let after = WalkState::Options {
                        index: next,
                        wrapper: wrapper.clone(),
                        done: true,
                        operands,
                    };
                    return self.consume(index, Some(&wrapper), Some(after));
                }
                if !done && word.starts_with('-') {
                    if let Some(option) = script_option(&wrapper, &word) {
                        return self.follow_script_option(index, &wrapper, &option);
                    }
                    match wrapper_option_arity(&wrapper, &word) {
                        OptionArity::Flag => {
                            let after = WalkState::Options {
                                index: next,
                                wrapper: wrapper.clone(),
                                done,
                                operands,
                            };
                            self.consume(index, Some(&wrapper), Some(after));
                        }
                        OptionArity::Value => {
                            let after = WalkState::Options {
                                index: (next + 1).min(self.tokens.len()),
                                wrapper: wrapper.clone(),
                                done,
                                operands,
                            };
                            // Both the option word and its value are consumed, so both
                            // get the fork rule: the option may carry a computed value
                            // (`-u$EMPTY`) and the separated value may be one (`-u $EMPTY`).
                            self.consume(index, Some(&wrapper), None);
                            self.consume(next, Some(&wrapper), Some(after));
                        }
                        OptionArity::Unknown => {
                            // Either a flag, or the next word is its value. The value
                            // reading is queued first so the flag reading — the primary
                            // one — is followed first.
                            if next < self.tokens.len() {
                                let value = WalkState::Options {
                                    index: next + 1,
                                    wrapper: wrapper.clone(),
                                    done,
                                    operands,
                                };
                                self.consume(next, Some(&wrapper), Some(value));
                            }
                            let flag = WalkState::Options {
                                index: next,
                                wrapper: wrapper.clone(),
                                done,
                                operands,
                            };
                            self.consume(index, Some(&wrapper), Some(flag));
                        }
                        OptionArity::Dynamic => {
                            // The option itself is computed, so no later word can be
                            // placed as one of `wrapper`'s options; `consume` still reads
                            // every later word as a place the program may start.
                            self.consume(index, Some(&wrapper), None);
                            return self.command(index, Some(wrapper));
                        }
                    }
                    return;
                }
                let operands_left = if wrapper == "env" && word.contains('=')
                    || wrapper == "timeout" && is_timeout_duration(&word)
                    || wrapper == "chrt" && is_ascii_digits(&word)
                {
                    Some(operands)
                } else if operands > 0 {
                    if wrapper == "flock" && is_ascii_digits(&word) && next == self.tokens.len() {
                        // `flock [options] FD` locks a descriptor the shell already
                        // opened; nothing runs.
                        return self.command(next, None);
                    }
                    Some(operands - 1)
                } else {
                    None
                };
                match operands_left {
                    // An operand of the wrapper itself — `chroot NEWROOT`, `taskset MASK`,
                    // `flock FILE`, `env VAR=value`, `timeout DURATION` — is consumed like
                    // any other of its words.
                    Some(operands) => {
                        let after = WalkState::Options {
                            index: next,
                            wrapper: wrapper.clone(),
                            done,
                            operands,
                        };
                        self.consume(index, Some(&wrapper), Some(after));
                    }
                    // Not one of the wrapper's own words: nothing is consumed, because
                    // this is where its program begins.
                    None => self.pending.push(WalkState::Program {
                        index,
                        wrapper: Some(wrapper),
                    }),
                }
            }
            WalkState::Script { index, wrapper } => {
                if let Some(option) = script_runner(&wrapper) {
                    self.script_candidates(index, &option, true);
                }
            }
        }
    }
}

/// Operands a wrapper takes before the program it runs: `chroot NEWROOT`,
/// `taskset MASK`, `flock FILE`. `env VAR=value`, `timeout DURATION` and
/// `chrt PRIORITY` are recognised by shape in [`WrapperWalk::follow`] instead.
fn wrapper_operands(wrapper: &str) -> u8 {
    match wrapper {
        "chroot" | "taskset" | "flock" => 1,
        _ => 0,
    }
}

/// Whether `word` is what `timeout` reads as its duration: a number with an optional
/// unit — `5`, `1.5m`, `10s`, `.5`, `+5`, `1e3` — or `inf`.
///
/// Only the start of the word decides. A word that starts with a digit is either a
/// duration `timeout` accepts or one it rejects — and then nothing runs — and no
/// program the gate knows starts with a digit, so reading every such word as a
/// duration never hides a program the gate would judge. Checking the letters instead
/// let `sh` and `dd` through as durations, because every letter of theirs is a unit
/// suffix: in `timeout 5 sh -c 'rm -rf /'` the shell was a second duration and `-c` an
/// option of `timeout`; in `timeout 5 dd of=/dev/sda` the program judged was
/// `of=/dev/sda`. Both ran.
fn is_timeout_duration(word: &str) -> bool {
    if word.eq_ignore_ascii_case("inf") || word.eq_ignore_ascii_case("infinity") {
        return true;
    }
    let number = word.strip_prefix('+').unwrap_or(word);
    let number = number.strip_prefix('.').unwrap_or(number);
    number.starts_with(|character: char| character.is_ascii_digit())
}

fn is_ascii_digits(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_digit())
}

/// A wrapper option whose value is a script for a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScriptOption {
    /// The runner named in findings.
    runner: &'static str,
    /// Whether the words after the script belong to it too: `env -S` appends them to the
    /// script as arguments, `flock -c` takes exactly one word.
    takes_rest: bool,
    /// The script when the option word carries it itself: `-S'rm -rf /'`, `-iS'rm -rf /'`,
    /// `--split-string='rm -rf /'`.
    attached: Option<String>,
}

/// The inline-script option of `wrapper`, in every spelling the program accepts.
///
/// Recognising only the exact standalone word left the script an ordinary value:
/// `env -iS 'rm -rf /'` and `env -S'rm -rf /'` were `Allow`, and behind another wrapper
/// `sudo env --split-string='rm -rf /'` was only a prompt. GNU `env` reads its short
/// options as a cluster, so any cluster of its flags ending in `S` introduces the script,
/// attached when text follows the `S` and the next word otherwise.
///
/// `flock` is read the same way for uniformity, and that is deliberately conservative:
/// util-linux 2.39.3 honours only the bare `-c`/`--command` word after the lock file and
/// hands `-c'…'`, `-nc` and `--command=…` to `execvp` as the command name, so those
/// spellings run nothing. A refusal for a line that cannot run costs a rewording.
fn script_option(wrapper: &str, option: &str) -> Option<ScriptOption> {
    let (letter, long, runner, takes_rest) = script_option_table(wrapper)?;
    let attached = if let Some(rest) = option.strip_prefix(long) {
        match rest.strip_prefix('=') {
            Some(script) => Some(script.to_owned()),
            None if rest.is_empty() => None,
            None => return None,
        }
    } else {
        let cluster = option
            .strip_prefix('-')
            .filter(|cluster| !cluster.starts_with('-'))?;
        let mut characters = cluster.char_indices();
        loop {
            let (index, character) = characters.next()?;
            if character == letter {
                let script = &cluster[index + character.len_utf8()..];
                break (!script.is_empty()).then(|| script.to_owned());
            }
            // Only a flag can precede the script option in a cluster: in `-uS` the `S`
            // is `-u`'s value, not `env`'s script option.
            if !wrapper_option_is_flag(wrapper, &format!("-{character}")) {
                return None;
            }
        }
    };
    Some(ScriptOption {
        runner,
        takes_rest,
        attached,
    })
}

/// The inline script `su` or a shell carries in the option word itself:
/// `su --command='rm -rf /'`, `su -c'rm -rf /'`, `su -lc'rm -rf /'`.
///
/// util-linux `su` reads `-c` with `getopt_long`, so its argument may be attached; the
/// word was read as an ordinary option instead and `su --command='rm -rf /'` reported
/// only that `su`'s command could not be checked. The script found here is judged in
/// addition to the words after the option, never instead of them, because a shell reads
/// the attached spelling as more flags — measured: `sh -c'rm -rf /'` is `Illegal option
/// -r` under dash and `invalid option` under bash — and only the following word is its
/// script.
fn attached_command_script(word: &str) -> Option<String> {
    if let Some(script) = word.strip_prefix("--command=") {
        return Some(script.to_owned());
    }
    let cluster = word
        .strip_prefix('-')
        .filter(|cluster| !cluster.starts_with('-'))?;
    let (_, script) = cluster.split_once('c')?;
    (!script.is_empty()).then(|| script.to_owned())
}

/// Whether `word` is the inline-script option of `su`, in any spelling.
fn is_su_command_option(word: &str) -> bool {
    is_command_script_option(word) || word.starts_with("--command=")
}

/// The inline-script option each wrapper has: its short letter, its long spelling, the
/// runner named in findings, and whether the words after the script belong to it.
fn script_option_table(wrapper: &str) -> Option<(char, &'static str, &'static str, bool)> {
    match wrapper {
        "env" => Some(('S', "--split-string", "env -S", true)),
        "flock" => Some(('c', "--command", "flock -c", false)),
        _ => None,
    }
}

/// The inline-script option `wrapper` has at all, in its standalone spelling.
///
/// A word the shell computes may expand to it, so the walk needs to know a wrapper has
/// one without having a word to read.
fn script_runner(wrapper: &str) -> Option<ScriptOption> {
    let (_, _, runner, takes_rest) = script_option_table(wrapper)?;
    Some(ScriptOption {
        runner,
        takes_rest,
        attached: None,
    })
}

/// How a wrapper reads one of its options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionArity {
    /// The word is complete: a flag, or an option with its value attached
    /// (`--user=root`, `-uroot`, `nice -10`).
    Flag,
    /// The next word is the option's value.
    Value,
    /// In neither table: the next word may or may not be its value.
    Unknown,
    /// The option itself is computed by the shell (`-$FLAGS`), so nothing after it
    /// can be placed.
    Dynamic,
}

/// What the gate knows about a wrapper's own options: which take their value from the
/// next word and which take none. An option in neither list forks the reading in
/// [`wrapper_readings`], so an entry missing here costs a confirmation, never a denial.
/// Short options are listed as `-x` and long ones as `--xxx`; `--xxx=value` and a
/// short cluster (`-Eu root`, `-uroot`) are read per option by [`wrapper_option_arity`].
struct WrapperOptions {
    wrapper: &'static str,
    value: &'static [&'static str],
    flag: &'static [&'static str],
}

const WRAPPER_OPTIONS: &[WrapperOptions] = &[
    WrapperOptions {
        wrapper: "sudo",
        value: &[
            "-C",
            "--close-from",
            "-D",
            "--chdir",
            "-g",
            "--group",
            "-h",
            "--host",
            "-p",
            "--prompt",
            "-r",
            "--role",
            "-R",
            "--chroot",
            "-t",
            "--type",
            "-T",
            "--command-timeout",
            "-u",
            "--user",
            "-U",
            "--other-user",
        ],
        flag: &[
            "-A",
            "--askpass",
            "-B",
            "--bell",
            "-b",
            "--background",
            "-E",
            "--preserve-env",
            "-e",
            "--edit",
            "-H",
            "--set-home",
            "-i",
            "--login",
            "-K",
            "--remove-timestamp",
            "-k",
            "--reset-timestamp",
            "-l",
            "--list",
            "-N",
            "--no-update",
            "-n",
            "--non-interactive",
            "-P",
            "--preserve-groups",
            "-S",
            "--stdin",
            "-s",
            "--shell",
            "-V",
            "-v",
            "--validate",
        ],
    },
    WrapperOptions {
        wrapper: "doas",
        value: &["-a", "-C", "-u"],
        flag: &["-L", "-n", "-s"],
    },
    WrapperOptions {
        wrapper: "env",
        value: &[
            "-a",
            "--argv0",
            "-C",
            "--chdir",
            "-S",
            "--split-string",
            "-u",
            "--unset",
        ],
        flag: &[
            "-i",
            "--ignore-environment",
            "-0",
            "--null",
            "-v",
            "--debug",
            "--block-signal",
            "--default-signal",
            "--ignore-signal",
            "--list-signal-handling",
        ],
    },
    WrapperOptions {
        wrapper: "nice",
        value: &["-n", "--adjustment"],
        flag: &[],
    },
    WrapperOptions {
        wrapper: "ionice",
        value: &[
            "-c",
            "--class",
            "-n",
            "--classdata",
            "-p",
            "--pid",
            "-P",
            "--pgid",
            "-u",
            "--uid",
        ],
        // `-t` ignores failures; listed as value-taking it swallowed the program.
        flag: &["-t", "--ignore"],
    },
    WrapperOptions {
        wrapper: "time",
        value: &["-f", "--format", "-o", "--output"],
        flag: &[
            "-a",
            "--append",
            "-p",
            "--portability",
            "-q",
            "--quiet",
            "-v",
            "--verbose",
        ],
    },
    WrapperOptions {
        wrapper: "timeout",
        value: &["-k", "--kill-after", "-s", "--signal"],
        flag: &["--foreground", "--preserve-status", "-v", "--verbose"],
    },
    WrapperOptions {
        wrapper: "nohup",
        value: &[],
        flag: &[],
    },
    WrapperOptions {
        wrapper: "xargs",
        value: &[
            "-a",
            "--arg-file",
            "-d",
            "--delimiter",
            "-E",
            "-I",
            "-L",
            "-n",
            "--max-args",
            "-P",
            "--max-procs",
            "--process-slot-var",
            "-s",
            "--max-chars",
        ],
        // `-e`, `-i`, `-l` and their long forms take an optional value that must be
        // attached (`-i{}`, `--eof=EOF`); on their own they take none, and
        // `xargs -i rm -rf /` runs `rm -rf /` once per input line.
        flag: &[
            "-0",
            "--null",
            "-e",
            "--eof",
            "-i",
            "--replace",
            "-l",
            "--max-lines",
            "-o",
            "--open-tty",
            "-p",
            "--interactive",
            "-r",
            "--no-run-if-empty",
            "--show-limits",
            "-t",
            "--verbose",
            "-x",
            "--exit",
        ],
    },
    WrapperOptions {
        wrapper: "command",
        value: &[],
        flag: &["-p", "-v", "-V"],
    },
    WrapperOptions {
        wrapper: "builtin",
        value: &[],
        flag: &[],
    },
    WrapperOptions {
        wrapper: "exec",
        value: &["-a"],
        flag: &["-c", "-l"],
    },
    WrapperOptions {
        wrapper: "setsid",
        value: &[],
        flag: &["-c", "--ctty", "-f", "--fork", "-w", "--wait"],
    },
    WrapperOptions {
        wrapper: "stdbuf",
        value: &["-i", "--input", "-o", "--output", "-e", "--error"],
        flag: &[],
    },
    WrapperOptions {
        wrapper: "chroot",
        value: &["--groups", "--userspec"],
        flag: &["--skip-chdir"],
    },
    WrapperOptions {
        wrapper: "watch",
        value: &["-n", "--interval"],
        // `-d`/`--differences` takes a value only as `--differences=permanent`.
        flag: &[
            "-b",
            "--beep",
            "-c",
            "--color",
            "-C",
            "--no-color",
            "-d",
            "--differences",
            "-e",
            "--errexit",
            "-g",
            "--chgexit",
            "-p",
            "--precise",
            "-q",
            "--no-wrap",
            "-r",
            "--no-rerun",
            "-t",
            "--no-title",
            "-w",
            "--no-linewrap",
            "-x",
            "--exec",
        ],
    },
    WrapperOptions {
        wrapper: "chrt",
        value: &[
            "-D",
            "--sched-deadline",
            "-P",
            "--sched-period",
            "-T",
            "--sched-runtime",
        ],
        flag: &[
            "-a",
            "--all-tasks",
            "-b",
            "--batch",
            "-d",
            "--deadline",
            "-f",
            "--fifo",
            "-i",
            "--idle",
            "-m",
            "--max",
            "-o",
            "--other",
            "-p",
            "--pid",
            "-R",
            "--reset-on-fork",
            "-r",
            "--rr",
            "-v",
            "--verbose",
        ],
    },
    WrapperOptions {
        wrapper: "taskset",
        value: &[],
        flag: &["-a", "--all-tasks", "-c", "--cpu-list", "-p", "--pid"],
    },
    WrapperOptions {
        wrapper: "flock",
        value: &[
            "-c",
            "--command",
            "-E",
            "--conflict-exit-code",
            "-w",
            "--wait",
            "--timeout",
        ],
        flag: &[
            "-e",
            "-x",
            "--exclusive",
            "-F",
            "--no-fork",
            "-n",
            "--nb",
            "--nonblock",
            "-o",
            "--close",
            "-s",
            "--shared",
            "-u",
            "--unlock",
            "--verbose",
        ],
    },
];

fn wrapper_options(wrapper: &str) -> Option<&'static WrapperOptions> {
    WRAPPER_OPTIONS
        .iter()
        .find(|entry| entry.wrapper == wrapper)
}

fn wrapper_option_takes_value(wrapper: &str, option: &str) -> bool {
    wrapper_options(wrapper).is_some_and(|entry| entry.value.contains(&option))
}

fn wrapper_option_is_flag(wrapper: &str, option: &str) -> bool {
    matches!(option, "--help" | "--version")
        || wrapper_options(wrapper).is_some_and(|entry| entry.flag.contains(&option))
}

/// How `wrapper` reads the option word `option`, which starts with `-`.
///
/// `--name=value` carries its value; a short cluster is read letter by letter the way
/// `getopt` does, so `-Eu root` is `-E` then `-u root` and `-uroot` is `-u` with its
/// value attached. Read as one unknown word, `-Eu` was a flag, `root` became the
/// program and `sudo -Eu root rm -rf /` was `Allow`.
fn wrapper_option_arity(wrapper: &str, option: &str) -> OptionArity {
    if let Some(long) = option.strip_prefix("--") {
        let name = long.split_once('=').map_or(long, |(name, _)| name);
        if name.chars().any(is_dynamic_char) {
            return OptionArity::Dynamic;
        }
        if name.len() != long.len() {
            return OptionArity::Flag;
        }
        return if wrapper_option_takes_value(wrapper, option) {
            OptionArity::Value
        } else if wrapper_option_is_flag(wrapper, option) {
            OptionArity::Flag
        } else {
            OptionArity::Unknown
        };
    }
    let cluster = option.strip_prefix('-').unwrap_or(option);
    if wrapper == "nice" && is_ascii_digits(cluster) {
        // `nice -10` is the adjustment itself.
        return OptionArity::Flag;
    }
    short_cluster_arity(wrapper, cluster)
}

/// [`wrapper_option_arity`] for the letters after a single `-`.
fn short_cluster_arity(wrapper: &str, cluster: &str) -> OptionArity {
    let mut letters = cluster.chars();
    let Some(letter) = letters.next() else {
        return OptionArity::Flag;
    };
    if is_dynamic_char(letter) {
        return OptionArity::Dynamic;
    }
    let rest = letters.as_str();
    let option = format!("-{letter}");
    if wrapper_option_takes_value(wrapper, &option) {
        // `-uroot` carries its value; `-u root` takes the next word.
        return if rest.is_empty() {
            OptionArity::Value
        } else {
            OptionArity::Flag
        };
    }
    if wrapper_option_is_flag(wrapper, &option) {
        return short_cluster_arity(wrapper, rest);
    }
    if rest.is_empty() {
        return OptionArity::Unknown;
    }
    // An unknown letter with more after it: as a flag the rest is more options, as a
    // value-taker the rest is its value and the word is complete.
    match short_cluster_arity(wrapper, rest) {
        OptionArity::Flag => OptionArity::Flag,
        OptionArity::Dynamic => OptionArity::Dynamic,
        OptionArity::Value | OptionArity::Unknown => OptionArity::Unknown,
    }
}

/// A character that makes the shell compute the word it is in; see
/// [`is_dynamic_literal`].
fn is_dynamic_char(character: char) -> bool {
    matches!(character, '$' | '`' | '*' | '?' | '[')
}

/// The command lines a resource runs *through* a wrapper or an inline shell script, each
/// analysed as a command of its own.
///
/// The permission layer matches one flattened command line, so under a `deny` written as
/// `rm -rf*` the resources `sh -c 'rm -rf /'`, `env rm -rf /` and `nice rm -rf /` were
/// the programs `sh`, `env` and `nice`, and answered Ask. This crate owns wrapper
/// semantics — [`unwrap_wrappers`], [`SHELL_COMMANDS`], `env -S`, `su -c`, `eval` — so it
/// is the layer that can say what such a call really runs. The shell tool adds every
/// returned resource's `source` to the patterns it asks the permission layer for. That
/// can only widen a deny: the engine refuses as soon as any pattern is denied, and a
/// pattern left at ask can turn an allow into a prompt, never a prompt or a deny into
/// anything weaker.
///
/// `xargs` is deliberately not followed: what comes after it is a command *prefix* whose
/// arguments arrive on standard input, not a command line, so reading it as one would
/// name a resource the call does not run. Nesting is bounded like the embedded-script
/// walk in [`assess_embedded_script`].
pub(crate) fn nested_command_resources(
    resource: &CommandResource,
    syntax: ShellSyntax,
) -> Vec<CommandResource> {
    let mut nested = Vec::new();
    collect_nested_commands(&resource.tokens, syntax, 0, &mut nested);
    nested
}

fn collect_nested_commands(
    tokens: &[String],
    syntax: ShellSyntax,
    depth: usize,
    nested: &mut Vec<CommandResource>,
) {
    if depth >= MAX_EMBEDDED_SCRIPT_DEPTH {
        return;
    }
    let Some(first) = tokens.first() else {
        return;
    };
    let word = |token: &String| static_shell_word(token, syntax);
    let program = command_name(first, syntax);
    let script = if program == "eval" {
        Some(tokens[1..].iter().map(word).collect::<Vec<_>>().join(" "))
    } else if SHELL_COMMANDS.contains(&program.as_str()) {
        tokens
            .iter()
            .position(|token| is_command_script_option(&word(token)))
            .and_then(|index| tokens.get(index + 1))
            .map(word)
    } else if program == "su" {
        tokens
            .iter()
            .position(|token| is_su_command_option(&word(token)))
            .and_then(|index| {
                attached_command_script(&word(&tokens[index]))
                    .or_else(|| tokens.get(index + 1).map(word))
            })
    } else if let Some(embedded) = env_split_script(tokens, syntax) {
        Some(embedded.iter().map(word).collect::<Vec<_>>().join(" "))
    } else {
        let (inner, wrapper) = unwrap_wrappers(tokens, syntax);
        if wrapper.is_none() || inner.is_empty() {
            return;
        }
        let wrappers = &tokens[..tokens.len() - inner.len()];
        if wrappers
            .iter()
            .any(|token| command_name(token, syntax) == "xargs")
        {
            return;
        }
        Some(inner.join(" "))
    };
    let Some(script) = script.filter(|script| !script.trim().is_empty()) else {
        return;
    };
    let Ok(analysis) = analyze_command(&script, syntax) else {
        return;
    };
    for inner in analysis.commands {
        collect_nested_commands(&inner.tokens, syntax, depth + 1, nested);
        nested.push(inner);
    }
}

fn assess_find(
    tokens: &[String],
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
    findings: &mut Vec<RiskFinding>,
) -> Result<(), ToolError> {
    let destructive = tokens.iter().any(|token| {
        matches!(
            unquote(token).to_ascii_lowercase().as_str(),
            "-delete" | "-exec" | "-execdir"
        )
    });
    if !destructive {
        return Ok(());
    }
    let mut options_done = false;
    let roots: Vec<String> = tokens
        .iter()
        .skip(1)
        .map(|token| unquote(token))
        .filter_map(|token| {
            if token == "--" {
                options_done = true;
                return None;
            }
            if !options_done && token.starts_with('-') {
                return None;
            }
            options_done = true;
            Some(token)
        })
        .take_while(|token| !token.starts_with('-'))
        .collect();
    if roots.is_empty() {
        findings.push(unknown_target_finding(
            "`find` performs deletion but its search root is unknown".to_owned(),
        ));
        return Ok(());
    }
    for root in roots {
        assess_destructive_target(&root, context, syntax, false, findings);
    }
    for index in tokens.iter().enumerate().filter_map(|(index, token)| {
        matches!(unquote(token).as_str(), "-exec" | "-execdir").then_some(index)
    }) {
        let command = tokens[index + 1..]
            .iter()
            .take_while(|token| !matches!(unquote(token).as_str(), ";" | "+"))
            .filter(|token| unquote(token) != "{}")
            .cloned()
            .collect::<Vec<_>>();
        assess_embedded_script(&command, syntax, context, depth, "find -exec", findings)?;
    }
    Ok(())
}

/// The arguments a destructive program will act on, each with one outer quote pair
/// removed and, under Bash, `$'…'`/`$"…"` rewritten as the plain quotes they denote so
/// `rm -rf $'/'` names `/`. Everything else stays as written for
/// [`assess_destructive_target`], which decides for itself what is dynamic.
fn destructive_targets(tokens: &[String], program: &str, syntax: ShellSyntax) -> Vec<String> {
    let argument = |token: &String| match syntax {
        ShellSyntax::Bash => unquote(&without_dollar_quotes(token)),
        ShellSyntax::PowerShell => unquote(token),
    };
    if program == "dd" {
        return tokens
            .iter()
            .skip(1)
            .filter_map(|token| argument(token).strip_prefix("of=").map(str::to_owned))
            .collect();
    }

    let mut targets = Vec::new();
    let mut options_done = false;
    let mut skip_value = false;
    for token in tokens.iter().skip(1) {
        let token = argument(token);
        if skip_value {
            skip_value = false;
            continue;
        }
        if !options_done && token == "--" {
            options_done = true;
            continue;
        }
        if !options_done && token.starts_with('-') {
            skip_value = option_consumes_value(program, &token);
            continue;
        }
        targets.push(token);
    }
    targets
}

fn source_mentions_home_target(source: &str) -> bool {
    source.split_ascii_whitespace().any(|token| {
        let token = unquote(token);
        matches!(token.as_str(), "$HOME" | "${HOME}")
    })
}

fn option_consumes_value(program: &str, option: &str) -> bool {
    matches!(
        (program, option),
        ("truncate", "-s" | "--size" | "-o" | "--io-blocks")
            | ("shred", "-n" | "--iterations" | "-s" | "--size")
            | ("remove-item", "-filter" | "-include" | "-exclude")
    )
}

fn assess_destructive_target(
    raw: &str,
    context: &RiskContext,
    syntax: ShellSyntax,
    absent_temp_file_cleanup: bool,
    findings: &mut Vec<RiskFinding>,
) {
    match static_brace_expansions(raw) {
        BraceExpansions::Expanded(targets) => {
            for target in targets {
                assess_destructive_target(
                    &target,
                    context,
                    syntax,
                    absent_temp_file_cleanup,
                    findings,
                );
            }
            return;
        }
        BraceExpansions::Unknown => {
            findings.push(confirm_finding(
                "target contains brace syntax that cannot be expanded statically".to_owned(),
                Some(raw.to_owned()),
            ));
            return;
        }
        BraceExpansions::Absent => {}
    }
    let expanded = expand_path(raw, context, syntax);
    if context.working_dir.is_none() && !is_rooted(&expanded) {
        findings.push(catastrophic_finding(
            "relative destructive target follows a directory change whose result is unknown"
                .to_owned(),
            raw.to_owned(),
        ));
        return;
    }
    if contains_glob(raw) {
        if glob_covers_protected_parent(&expanded, context) {
            findings.push(catastrophic_finding(
                "would destroy the contents of a protected directory".to_owned(),
                raw.to_owned(),
            ));
        } else {
            findings.push(confirm_finding(
                "target contains a glob, so its exact blast radius is unknown".to_owned(),
                Some(raw.to_owned()),
            ));
        }
        return;
    }
    if is_dynamic_path(raw) && !home_is_fully_expanded(raw, context) {
        findings.push(confirm_finding(
            "target is computed at runtime, so its value cannot be checked before execution"
                .to_owned(),
            Some(raw.to_owned()),
        ));
        return;
    }
    if is_catastrophic_target(&expanded, context) {
        findings.push(catastrophic_finding(
            "targets a protected system, home, credential, or device path".to_owned(),
            zuno_paths::wire_path(&expanded),
        ));
        return;
    }
    if absent_temp_file_cleanup
        && is_inside_system_temp(&expanded)
        && matches!(
            std::fs::symlink_metadata(&expanded),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    {
        return;
    }
    findings.push(confirm_finding(
        "irreversibly removes or overwrites data".to_owned(),
        Some(zuno_paths::wire_path(&expanded)),
    ));
}

fn is_forced_non_recursive_rm(program: &str, tokens: &[String]) -> bool {
    if program != "rm" {
        return false;
    }
    let mut force = false;
    for token in tokens.iter().skip(1).map(|token| unquote(token)) {
        if token == "--" {
            break;
        }
        if token == "--force" {
            force = true;
            continue;
        }
        if matches!(token.as_str(), "--recursive" | "--dir") {
            return false;
        }
        let Some(flags) = token
            .strip_prefix('-')
            .filter(|flags| !flags.starts_with('-'))
        else {
            continue;
        };
        if flags.contains(['r', 'R', 'd']) {
            return false;
        }
        force |= flags.contains('f');
    }
    force
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BraceExpansions {
    Absent,
    Expanded(Vec<String>),
    Unknown,
}

fn static_brace_expansions(raw: &str) -> BraceExpansions {
    let Some(open) = raw
        .match_indices('{')
        .map(|(index, _)| index)
        .find(|&index| index == 0 || raw.as_bytes()[index - 1] != b'$')
    else {
        return BraceExpansions::Absent;
    };
    let Some(relative_close) = raw[open + 1..].find('}') else {
        return BraceExpansions::Unknown;
    };
    let close = open + 1 + relative_close;
    let body = &raw[open + 1..close];
    if body.contains(['{', '}']) {
        return BraceExpansions::Unknown;
    }
    let alternatives = body.split(',').collect::<Vec<_>>();
    if alternatives.len() < 2 {
        return BraceExpansions::Unknown;
    }
    let prefix = &raw[..open];
    let suffix = &raw[close + 1..];
    BraceExpansions::Expanded(
        alternatives
            .into_iter()
            .map(|alternative| format!("{prefix}{alternative}{suffix}"))
            .collect(),
    )
}

fn assess_redirect_target(
    raw: &str,
    context: &RiskContext,
    syntax: ShellSyntax,
    findings: &mut Vec<RiskFinding>,
) {
    let unquoted = unquote(raw);
    if matches!(
        unquoted.as_str(),
        "/dev/null" | "/dev/stdout" | "/dev/stderr"
    ) || unquoted.starts_with("/dev/fd/")
        || unquoted.starts_with('&')
    {
        return;
    }
    if is_dynamic_path(&unquoted) && !home_is_fully_expanded(&unquoted, context) {
        findings.push(confirm_finding(
            "output redirection has a runtime-computed target".to_owned(),
            Some(unquoted),
        ));
        return;
    }
    let expanded = expand_path(&unquoted, context, syntax);
    if is_catastrophic_target(&expanded, context) {
        findings.push(catastrophic_finding(
            "output redirection would overwrite a protected path or device node".to_owned(),
            zuno_paths::wire_path(&expanded),
        ));
        return;
    }
    let outside_working_dir = context
        .working_dir
        .as_deref()
        .is_none_or(|cwd| !expanded.starts_with(normalize_path(cwd)));
    match std::fs::symlink_metadata(&expanded) {
        Ok(_) => findings.push(confirm_finding(
            "output redirection would replace an existing path".to_owned(),
            Some(zuno_paths::wire_path(&expanded)),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if outside_working_dir && !is_inside_system_temp(&expanded) {
                findings.push(confirm_finding(
                    "output redirection would create a file outside the working directory"
                        .to_owned(),
                    Some(zuno_paths::wire_path(&expanded)),
                ));
            }
        }
        Err(_) => findings.push(confirm_finding(
            "output redirection target could not be inspected safely".to_owned(),
            Some(zuno_paths::wire_path(&expanded)),
        )),
    }
}

fn is_inside_system_temp(path: &Path) -> bool {
    let temp = std::env::temp_dir();
    let temp = temp
        .canonicalize()
        .unwrap_or_else(|_| normalize_path(&temp));
    let mut ancestor = path.parent();
    while let Some(candidate) = ancestor {
        match candidate.canonicalize() {
            Ok(resolved) => return resolved.starts_with(&temp),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = candidate.parent();
            }
            Err(_) => return false,
        }
    }
    false
}

fn truncating_redirect_targets(source: &str) -> Vec<String> {
    let characters: Vec<char> = source.chars().collect();
    let mut targets = Vec::new();
    let mut index = 0;
    let mut quote = None;
    while index < characters.len() {
        let character = characters[index];
        if character == '\\' && quote != Some('\'') {
            index = index.saturating_add(2);
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            index += 1;
            continue;
        }
        if character != '>' || quote.is_some() {
            index += 1;
            continue;
        }
        index += 1;
        if characters.get(index) == Some(&'>') || characters.get(index) == Some(&'|') {
            index += 1;
        }
        while characters
            .get(index)
            .is_some_and(|character| character.is_whitespace())
        {
            index += 1;
        }
        let start = index;
        let mut target_quote = None;
        while let Some(character) = characters.get(index).copied() {
            if character == '\\' && target_quote != Some('\'') {
                index = index.saturating_add(2);
                continue;
            }
            if matches!(character, '\'' | '"') {
                if target_quote == Some(character) {
                    target_quote = None;
                } else if target_quote.is_none() {
                    target_quote = Some(character);
                }
                index += 1;
                continue;
            }
            if target_quote.is_none()
                && (character.is_whitespace() || matches!(character, ';' | '&' | '|'))
            {
                break;
            }
            index += 1;
        }
        if start < index {
            targets.push(characters[start..index].iter().collect());
        }
    }
    targets
}

fn expand_path(raw: &str, context: &RiskContext, syntax: ShellSyntax) -> PathBuf {
    let raw = static_shell_word(raw, syntax);
    let expanded_home = context
        .home_dir
        .as_deref()
        .map(|home| expand_home_spellings(&raw, home, &home.to_string_lossy()));
    let mut text = expanded_home.unwrap_or(raw);
    if let Some(home) = &context.home_dir {
        if text == "~" {
            text = home.to_string_lossy().into_owned();
        } else if let Some(rest) = text.strip_prefix("~/") {
            text = home.join(rest).to_string_lossy().into_owned();
        }
    }
    let path = PathBuf::from(text);
    if is_rooted(&path) {
        normalize_path(&path)
    } else if let Some(cwd) = &context.working_dir {
        normalize_path(&cwd.join(path))
    } else {
        normalize_path(&path)
    }
}

/// Replaces every shell spelling of the home directory with `replacement`.
///
/// The Windows spellings are applied only when the home directory is itself a Windows
/// path: `%USERPROFILE%` is an ordinary literal in a POSIX shell, and expanding it
/// there would invent a home reference the shell will never make.
fn expand_home_spellings(raw: &str, home: &Path, replacement: &str) -> String {
    let mut text = raw
        .replace("${HOME}", replacement)
        .replace("$HOME", replacement);
    if windows_target(home).is_some() {
        for spelling in WINDOWS_HOME_SPELLINGS {
            text = replace_ignoring_case(&text, spelling, replacement);
        }
    }
    text
}

/// Replaces every occurrence of a lowercase ASCII `needle`, ignoring case.
///
/// Windows environment variable names are case-insensitive, so `%UserProfile%` and
/// `%USERPROFILE%` name the same directory and must expand the same way.
/// [`str::to_ascii_lowercase`] changes no byte lengths, so the folded copy's match
/// offsets index the original text exactly.
fn replace_ignoring_case(text: &str, needle: &str, replacement: &str) -> String {
    debug_assert_eq!(needle, needle.to_ascii_lowercase(), "needle must be folded");
    let folded = text.to_ascii_lowercase();
    let mut replaced = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(offset) = folded[index..].find(needle) {
        let start = index + offset;
        replaced.push_str(&text[index..start]);
        replaced.push_str(replacement);
        index = start + needle.len();
    }
    replaced.push_str(&text[index..]);
    replaced
}

fn normalize_path(path: &Path) -> PathBuf {
    let absolute = path.is_absolute();
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir => normalized.push(Path::new("/")),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }
    if absolute && normalized.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        normalized
    }
}

/// A Windows absolute path split into the root it names and the part below that root.
///
/// The remainder is lowercased with `/` separators so the protected tables can be
/// compared as text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WindowsTarget {
    /// A drive-rooted path. `relative` is empty for the drive root itself.
    Drive { relative: String },
    /// A UNC share. `relative` is empty for the share root itself.
    Share { relative: String },
}

/// Splits a Windows absolute path into its root and drive-relative remainder.
///
/// Returns `None` for anything that does not name a Windows root, so POSIX assessment
/// is left exactly as it was.
///
/// This works on the rendered path rather than [`Path::components`] on purpose. Off
/// Windows, `Path::new("C:/Windows")` yields no [`Component::Prefix`] at all, so a
/// component-based matcher could not be exercised on a Linux or macOS host and would
/// silently diverge from the code that ships. Rendering also folds the extended
/// `\\?\` and `\\.\` prefixes and the verbatim `UNC\` form, which a caller can use to
/// spell the same protected directory three ways.
fn windows_target(path: &Path) -> Option<WindowsTarget> {
    let unified = path.to_string_lossy().replace('\\', "/");
    let (rooted, verbatim) = match unified
        .strip_prefix("//?/")
        .or_else(|| unified.strip_prefix("//./"))
    {
        Some(rest) => (rest, true),
        None => (unified.as_str(), false),
    };
    if let Some(share) = strip_unc_root(rooted, verbatim) {
        let mut segments = share.split('/').filter(|segment| !segment.is_empty());
        let _server = segments.next()?;
        let _share = segments.next()?;
        return Some(WindowsTarget::Share {
            relative: fold_segments(segments),
        });
    }
    let mut segments = rooted.split('/');
    if !is_drive_letter(segments.next()?) {
        return None;
    }
    Some(WindowsTarget::Drive {
        relative: fold_segments(segments.filter(|segment| !segment.is_empty())),
    })
}

/// The `server/share/...` remainder of a UNC path, in either spelling.
///
/// The bare `UNC\` spelling is only accepted after a verbatim prefix was stripped:
/// on its own, `unc/project/notes` is an ordinary relative POSIX path and must not be
/// mistaken for a network share root.
fn strip_unc_root(rooted: &str, verbatim: bool) -> Option<&str> {
    if let Some(rest) = rooted.strip_prefix("//") {
        return Some(rest);
    }
    if !verbatim {
        return None;
    }
    rooted
        .split_once('/')
        .filter(|(head, _)| head.eq_ignore_ascii_case("UNC"))
        .map(|(_, rest)| rest)
}

/// Whether a path names a root — on this platform, or on Windows.
///
/// [`Path::is_absolute`] answers only for the host. Off Windows, `C:\Windows` and
/// `\\server\share` are ordinary relative names, so a Windows target would be joined
/// onto a POSIX working directory and would stop matching the protected tables
/// entirely. The assessor has to classify a path the way the shell that runs it will,
/// and answering for both spellings also keeps the Windows rules exercisable by tests
/// on any host, which a `cfg(windows)` branch would not.
fn is_rooted(path: &Path) -> bool {
    path.is_absolute() || windows_target(path).is_some()
}

fn is_drive_letter(segment: &str) -> bool {
    let mut characters = segment.chars();
    characters
        .next()
        .is_some_and(|letter| letter.is_ascii_alphabetic())
        && characters.next() == Some(':')
        && characters.next().is_none()
}

/// Joins path segments with `/` and folds case, matching the protected tables.
fn fold_segments<'a>(segments: impl Iterator<Item = &'a str>) -> String {
    segments
        .filter(|segment| *segment != ".")
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("/")
}

/// The table spelling of a protected subpath: `/` separators, folded case.
fn folded_subpath(subpath: &str) -> String {
    subpath.replace('\\', "/").to_ascii_lowercase()
}

fn is_at_or_below(relative: &str, protected: &str) -> bool {
    relative == protected
        || relative
            .strip_prefix(protected)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether a Windows path names a protected system, profile, or credential location.
///
/// The home half reuses [`CREDENTIAL_SUBPATHS`] and [`PROTECTED_HOME_SUBPATHS`]
/// because `.ssh`, `.aws`, and `.config/gh` are spelled identically under
/// `C:\Users\alice`; only the comparison changes, since a Windows path is
/// case-insensitive and can be written with either separator.
fn is_catastrophic_windows_target(target: &WindowsTarget, context: &RiskContext) -> bool {
    let relative = match target {
        // A share root is the whole of someone else's filesystem. Nothing below it is
        // classified here: a project checked out on a share is ordinary work.
        WindowsTarget::Share { relative } => return relative.is_empty(),
        WindowsTarget::Drive { relative } => relative.as_str(),
    };
    if relative.is_empty()
        || PROTECTED_WINDOWS_SUBPATHS.contains(&relative)
        || RECURSIVE_WINDOWS_SUBPATHS
            .iter()
            .any(|protected| is_at_or_below(relative, protected))
    {
        return true;
    }
    let Some(WindowsTarget::Drive { relative: home }) = context
        .home_dir
        .as_deref()
        .map(normalize_path)
        .as_deref()
        .and_then(windows_target)
    else {
        return false;
    };
    if relative == home {
        return true;
    }
    let Some(inside) = relative
        .strip_prefix(home.as_str())
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return false;
    };
    CREDENTIAL_SUBPATHS
        .iter()
        .any(|subpath| is_at_or_below(inside, &folded_subpath(subpath)))
        || PROTECTED_HOME_SUBPATHS
            .iter()
            .any(|subpath| inside == folded_subpath(subpath))
}

fn is_catastrophic_target(path: &Path, context: &RiskContext) -> bool {
    let path = normalize_path(path);
    if let Some(target) = windows_target(&path) {
        return is_catastrophic_windows_target(&target, context);
    }
    if PROTECTED_SYSTEM_PATHS
        .iter()
        .any(|protected| path == Path::new(protected))
        || RECURSIVE_SYSTEM_PATHS
            .iter()
            .any(|protected| path.starts_with(protected))
    {
        return true;
    }
    let Some(home) = context.home_dir.as_deref().map(normalize_path) else {
        return false;
    };
    if path == home {
        return true;
    }
    if CREDENTIAL_SUBPATHS
        .iter()
        .any(|subpath| path.starts_with(home.join(subpath)))
    {
        return true;
    }
    PROTECTED_HOME_SUBPATHS
        .iter()
        .any(|subpath| path == home.join(subpath))
}

fn glob_covers_protected_parent(expanded: &Path, context: &RiskContext) -> bool {
    expanded.parent().is_some_and(|parent| {
        is_catastrophic_target(parent, context)
            && expanded
                .file_name()
                .is_some_and(|name| matches!(name.to_str(), Some("*") | Some("?")))
    })
}

fn home_is_fully_expanded(raw: &str, context: &RiskContext) -> bool {
    let Some(home) = context.home_dir.as_deref() else {
        return false;
    };
    let without_home = expand_home_spellings(raw, home, "");
    let has_other_expansion = without_home.contains('$') || raw.contains('`') || raw.contains("$(");
    let has_named_home = raw.starts_with('~') && raw != "~" && !raw.starts_with("~/");
    !has_other_expansion && !has_named_home
}

fn is_dynamic_path(path: &str) -> bool {
    path.starts_with("~") && path != "~" && !path.starts_with("~/")
        || path.starts_with('(')
        || path.starts_with("@(")
        || path.contains("$(")
        || path.contains("${")
        || path.contains('`')
        || path.contains('$')
        || path.contains('[')
}

fn contains_glob(path: &str) -> bool {
    let path = without_verbatim_prefix(path);
    path.contains('*') || path.contains('?')
}

/// The path with a Windows verbatim or device prefix removed.
///
/// `\\?\` and `\\.\` spell a root, so their `?` is not a wildcard. Leaving them in
/// place made every verbatim path look like a glob, and `\\?\C:\Windows` was reported
/// as an unknown blast radius instead of the protected directory it names. A `?`
/// anywhere else is still a glob: Windows file names cannot contain one.
fn without_verbatim_prefix(path: &str) -> &str {
    let mut characters = path.chars();
    let separator = |character: Option<char>| matches!(character, Some('/' | '\\'));
    if separator(characters.next())
        && separator(characters.next())
        && matches!(characters.next(), Some('?' | '.'))
        && separator(characters.next())
    {
        return &path[4..];
    }
    path
}

fn is_destructive_command(program: &str) -> bool {
    DESTRUCTIVE_COMMANDS.contains(&program) || program.starts_with("mkfs.")
}

/// The lowercase program name a command token denotes, so `"/usr/bin/RG"` and
/// `C:\Tools\RG.EXE` both name the same program as `rg`.
///
/// Which characters separate path segments is a property of the shell, not of the
/// host: under PowerShell a backslash is a separator, while under Bash it is an escape
/// (`r\m` runs `rm`). Reducing a PowerShell token with the POSIX rule turned
/// `C:\Windows\System32\bash.exe` into `C:WindowsSystem32bash.exe`, which matches
/// nothing, so an absolutely spelled nested shell escaped [`SHELL_COMMANDS`], a
/// wrapper escaped [`WRAPPER_COMMANDS`], and a destructive program escaped
/// [`DESTRUCTIVE_COMMANDS`] — on Windows only. [`Path::file_name`] is not used because
/// it recognizes `\` as a separator only when compiled for Windows.
///
/// A `.exe` or `.com` suffix is dropped so `rm.exe` and `format.com` reach the same
/// tables as `rm` and `format`. Script suffixes are deliberately left alone: a
/// `deploy.cmd` is its own program, not the tool it happens to be named after.
pub(crate) fn command_name(token: &str, syntax: ShellSyntax) -> String {
    let literal = static_shell_word(token, syntax);
    let separators: &[char] = match syntax {
        ShellSyntax::Bash => &['/'],
        ShellSyntax::PowerShell => &['/', '\\'],
    };
    let trimmed = literal.trim_end_matches(separators);
    let name = match trimmed.rsplit(separators).next() {
        Some(name) if !name.is_empty() => name,
        _ => literal.as_str(),
    }
    .to_ascii_lowercase();
    match name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".com"))
    {
        Some(stem) if !stem.is_empty() => stem.to_owned(),
        _ => name,
    }
}

/// The literal a shell word denotes, with that shell's own escape character.
///
/// Escaping is per shell and the difference decides whether a Windows path survives
/// assessment at all. Bash removes a backslash and keeps the next character; in
/// PowerShell a backslash is never an escape — its escape character is a backtick —
/// so a backslash there is a path separator. Reducing both with the POSIX rule
/// rewrote `'C:\Users\alice\.ssh'` to `C:Usersalice.ssh`, which matches no protected
/// directory, so the credential, profile, and system rules could not fire on the one
/// platform that spells paths that way.
fn static_shell_word(text: &str, syntax: ShellSyntax) -> String {
    let literal = match syntax {
        ShellSyntax::Bash => without_dollar_quotes(text),
        ShellSyntax::PowerShell => text.to_owned(),
    };
    reduce_shell_word(&literal, Some(shell_escape(syntax)))
}

/// Bash's `$'…'` and `$"…"` quoting, materialised as the literal it denotes.
///
/// `$'…'` is ANSI-C quoting and `$"…"` is locale translation, so `rm$''`, `r$'m'` and
/// `$'rm'` all name `rm`. Read as ordinary characters they produced the program `rm$`,
/// which matched no destructive table, so `rm$'' -rf /` ran without a finding.
///
/// An ANSI-C body's escapes are expanded, because an escape is the whole point of the
/// form: it can spell any byte, a newline included. Refusing a body that held a `\` left
/// `$'echo hi\nrm -rf /'` as written, and the `$` it kept made the word merely computed,
/// so `sh -c $'echo hi\nrm -rf /'` was a prompt while the identical
/// `sh -c 'echo hi; rm -rf /'` was refused — the nested parse never saw the second
/// command. The materialised text is re-escaped for [`reduce_shell_word`], which the
/// callers apply next, so the literal survives exactly. A body that expands to a `$` or a
/// backtick still marks the word computed downstream, and `$"…"` with either in its body
/// is left as written because the shell expands those itself.
///
/// Only a `$` outside every quote opens one of these forms; inside `'…'` or `"…"` it is
/// the character it appears to be. This is a reading for the deny side: it can only
/// make a word name a program or a path the shell would also name.
fn without_dollar_quotes(text: &str) -> String {
    let mut literal = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if escaped {
            literal.push(character);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                }
            }
            Some(_) => {
                if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    quote = None;
                }
            }
            None => {
                if character == '\\' {
                    escaped = true;
                } else if character == '$'
                    && let Some(&(_, opener @ ('\'' | '"'))) = characters.peek()
                    && let Some((body, end)) = dollar_quote(&text[index + 2..], opener)
                {
                    push_escaped(&mut literal, &body);
                    let resume = index + 2 + end + 1;
                    while characters.next_if(|&(next, _)| next < resume).is_some() {}
                    continue;
                } else if matches!(character, '\'' | '"') {
                    quote = Some(character);
                }
            }
        }
        literal.push(character);
    }
    literal
}

/// The literal a `$'…'` or `$"…"` body denotes and the byte offset of the quote that
/// closes it, or `None` when the body is unterminated or holds something only the shell
/// can resolve.
///
/// An ANSI-C body is expanded escape by escape. A translated body is taken verbatim, and
/// only when it holds no escape and no expansion, because the shell resolves those first.
fn dollar_quote(body: &str, opener: char) -> Option<(String, usize)> {
    if opener == '"' {
        let end = body.find('"')?;
        let inside = &body[..end];
        return (!inside.contains(['\\', '$', '`'])).then(|| (inside.to_owned(), end));
    }
    let mut literal = String::with_capacity(body.len());
    let mut index = 0;
    while let Some(character) = body[index..].chars().next() {
        match character {
            '\'' => return Some((literal, index)),
            '\\' => {
                let (expanded, consumed) = ansi_c_escape(&body[index + 1..]);
                literal.push_str(&expanded);
                index += 1 + consumed;
            }
            _ => {
                literal.push(character);
                index += character.len_utf8();
            }
        }
    }
    None
}

/// The text one ANSI-C escape after a `\` denotes, and the bytes it consumes.
///
/// Bash keeps an escape it does not define as written, so `$'\q'` is `\q`. A `\0` names
/// the byte no argument can carry, so it expands to nothing rather than to a `NUL` the
/// nested parse would have to carry.
fn ansi_c_escape(rest: &str) -> (String, usize) {
    let Some(escape) = rest.chars().next() else {
        // A trailing backslash is the character itself.
        return ("\\".to_owned(), 0);
    };
    let literal = escape.len_utf8();
    let simple = match escape {
        'a' => Some('\u{7}'),
        'b' => Some('\u{8}'),
        'e' | 'E' => Some('\u{1b}'),
        'f' => Some('\u{c}'),
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'v' => Some('\u{b}'),
        '\\' | '\'' | '"' | '?' => Some(escape),
        _ => None,
    };
    if let Some(character) = simple {
        return (character.to_string(), literal);
    }
    let numeric = match escape {
        'x' => radix_escape(&rest[1..], 16, 2).map(|(value, digits)| (value, digits + 1)),
        'u' => radix_escape(&rest[1..], 16, 4).map(|(value, digits)| (value, digits + 1)),
        'U' => radix_escape(&rest[1..], 16, 8).map(|(value, digits)| (value, digits + 1)),
        '0'..='7' => radix_escape(rest, 8, 3),
        'c' => {
            // `\cX` is the control character `X` names.
            return rest[literal..].chars().next().map_or_else(
                || ("\\c".to_owned(), literal),
                |control| {
                    let byte = u32::from(control.to_ascii_uppercase()) ^ 0x40;
                    (
                        char::from_u32(byte).map(String::from).unwrap_or_default(),
                        literal + control.len_utf8(),
                    )
                },
            );
        }
        _ => None,
    };
    match numeric {
        Some((value, consumed)) => (
            char::from_u32(value)
                .filter(|character| *character != '\0')
                .map(String::from)
                .unwrap_or_default(),
            consumed,
        ),
        None => (format!("\\{escape}"), literal),
    }
}

/// The value of at most `most` digits of `radix` at the front of `text`, and how many
/// digits it read.
fn radix_escape(text: &str, radix: u32, most: usize) -> Option<(u32, usize)> {
    let digits = text
        .chars()
        .take(most)
        .take_while(|character| character.is_digit(radix))
        .count();
    (digits > 0).then(|| {
        (
            u32::from_str_radix(&text[..digits], radix).unwrap_or_default(),
            digits,
        )
    })
}

/// Appends `literal` so that [`reduce_shell_word`] gives it back unchanged.
///
/// The materialised text may hold the quotes and backslashes the escapes named, and the
/// callers hand what this function builds to the shell-word reader next; without the
/// escaping, `$'it\'s'` would reopen a quote and swallow the rest of the word.
fn push_escaped(text: &mut String, literal: &str) {
    for character in literal.chars() {
        if matches!(character, '\\' | '\'' | '"') {
            text.push('\\');
        }
        text.push(character);
    }
}

/// Why a command whose program the shell must compute is only confirmable.
const DYNAMIC_PROGRAM: &str =
    "command name is computed at runtime, so destructive behavior cannot be checked";

/// Whether the shell has to compute `token` before it names anything.
///
/// A `$` — a parameter, `$(…)` or `${…}` — a backtick, or a glob character anywhere in
/// the word once quoting is removed means the program (or subcommand) that runs is not
/// the text that was written. The previous check looked only at the first character of
/// the first whitespace-delimited word, so `rm${IFS}-rf${IFS}/` was `rm` followed by
/// nothing dynamic and `rm$'' -rf /` was a program called `rm$`. The reading
/// over-approximates on purpose: a single-quoted `'$HOME'` is a literal name to the
/// shell, but reading it as dynamic costs one confirmation, while reading a dynamic
/// word as literal costs the catastrophic table.
fn is_dynamic_word(token: &str, syntax: ShellSyntax) -> bool {
    is_dynamic_literal(&static_shell_word(token, syntax), syntax)
}

/// [`is_dynamic_word`] for a word that has already been reduced.
///
/// PowerShell's `?` is the `Where-Object` alias, a fixed cmdlet and not a glob, so a
/// pipeline stage spelled that way stays static; every other `?` is a wildcard.
fn is_dynamic_literal(word: &str, syntax: ShellSyntax) -> bool {
    if syntax == ShellSyntax::PowerShell && word == "?" {
        return false;
    }
    word.contains(['$', '`', '*', '?', '['])
}

/// The escape character outside single quotes for each supported shell.
fn shell_escape(syntax: ShellSyntax) -> char {
    match syntax {
        ShellSyntax::Bash => '\\',
        ShellSyntax::PowerShell => '`',
    }
}

/// Removes shell quoting, and `escape` characters when the shell has one.
fn reduce_shell_word(text: &str, escape: Option<char>) -> String {
    let mut word = String::with_capacity(text.len());
    let mut quote = None;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if Some(character) == escape && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                word.push(character);
            }
            continue;
        }
        word.push(character);
    }
    if let (true, Some(escape)) = (escaped, escape) {
        word.push(escape);
    }
    word
}

/// One matched outer quote pair removed, and nothing else.
///
/// This is the reading for an argument the caller wrote as one quoted word, such as a
/// path. It is **not** the reading for a program, an option or a nested script — the
/// shell removes every quote in a word, so `p''ush` is `push` to it and stays `p''ush`
/// here; those go through [`static_shell_word`].
pub(crate) fn unquote(text: &str) -> String {
    if text.len() >= 2 {
        let first = text.as_bytes()[0];
        let last = text.as_bytes()[text.len() - 1];
        if matches!(first, b'\'' | b'"') && first == last {
            return text[1..text.len() - 1].to_owned();
        }
    }
    text.to_owned()
}

fn catastrophic_finding(reason: String, target: String) -> RiskFinding {
    RiskFinding {
        level: RiskLevel::Catastrophic,
        kind: RiskKind::Generic,
        reason,
        target: Some(target),
    }
}

fn confirm_finding(reason: String, target: Option<String>) -> RiskFinding {
    RiskFinding {
        level: RiskLevel::Confirm,
        kind: RiskKind::Generic,
        reason,
        target,
    }
}

fn git_history_finding(reason: String) -> RiskFinding {
    RiskFinding {
        level: RiskLevel::Confirm,
        kind: RiskKind::GitHistoryRewrite,
        reason,
        target: None,
    }
}

fn unknown_target_finding(reason: String) -> RiskFinding {
    confirm_finding(reason, None)
}

#[cfg(test)]
mod home_tests {
    use super::*;

    #[test]
    fn an_explicit_home_variable_wins_over_the_platform_home() {
        let home = home_directory_from(
            Some(OsString::from("/opt/relocated")),
            Some(PathBuf::from("/home/alice")),
        );
        assert_eq!(home, Some(PathBuf::from("/opt/relocated")));
    }

    #[test]
    fn an_absent_or_empty_home_variable_falls_back_to_the_platform_home() {
        // The Windows case: `cmd` and PowerShell set no `HOME`, so without the
        // fallback every home and credential rule below switched itself off.
        assert_eq!(
            home_directory_from(None, Some(PathBuf::from(r"C:\Users\alice"))),
            Some(PathBuf::from(r"C:\Users\alice"))
        );
        assert_eq!(
            home_directory_from(
                Some(OsString::new()),
                Some(PathBuf::from(r"C:\Users\alice"))
            ),
            Some(PathBuf::from(r"C:\Users\alice"))
        );
        assert_eq!(home_directory_from(None, None), None);
    }

    #[test]
    fn a_windows_root_is_recognized_in_every_spelling() {
        for spelling in [
            r"C:\Windows\System32",
            "C:/Windows/System32",
            r"c:/WINDOWS\system32",
        ] {
            assert_eq!(
                windows_target(Path::new(spelling)),
                Some(WindowsTarget::Drive {
                    relative: "windows/system32".to_owned()
                }),
                "{spelling}"
            );
        }
        for spelling in [r"C:\", "C:/", "C:"] {
            assert_eq!(
                windows_target(Path::new(spelling)),
                Some(WindowsTarget::Drive {
                    relative: String::new()
                }),
                "{spelling}"
            );
        }
        assert_eq!(
            windows_target(Path::new(r"\\?\C:\Windows")),
            Some(WindowsTarget::Drive {
                relative: "windows".to_owned()
            })
        );
        assert_eq!(
            windows_target(Path::new(r"\\server\share\project")),
            Some(WindowsTarget::Share {
                relative: "project".to_owned()
            })
        );
        assert_eq!(
            windows_target(Path::new(r"\\?\UNC\server\share")),
            Some(WindowsTarget::Share {
                relative: String::new()
            })
        );
    }

    #[test]
    fn a_posix_path_is_never_read_as_a_windows_root() {
        for path in ["/etc", "/", "relative/path", "unc/project/notes"] {
            let normalized = normalize_path(Path::new(path));
            assert_eq!(windows_target(&normalized), None, "{path}");
        }
    }

    /// A leading `//` means whatever the host's own path parser says it means.
    ///
    /// `normalize_path` walks [`Path::components`], and prefix parsing there belongs to
    /// the host: Windows reads the leading `//` as a UNC prefix and keeps the share,
    /// while on Linux `//` collapses to `/` and the result is the ordinary directory
    /// `/server/share`. Each answer is the one the shell that would run the command
    /// makes, and classifying the target the way that shell does is the assessor's whole
    /// job — so this case is pinned per host instead of being asserted away on one of
    /// them.
    #[test]
    fn a_double_slash_root_is_classified_the_way_the_host_reads_it() {
        let normalized = normalize_path(Path::new("//server/share"));
        #[cfg(windows)]
        assert_eq!(
            windows_target(&normalized),
            Some(WindowsTarget::Share {
                relative: String::new()
            })
        );
        #[cfg(not(windows))]
        assert_eq!(windows_target(&normalized), None);
    }

    #[test]
    fn the_windows_home_spellings_expand_only_when_home_is_a_windows_path() {
        let windows = Path::new(r"C:\Users\alice");
        for spelling in [
            "%USERPROFILE%",
            "%UserProfile%",
            "$env:USERPROFILE",
            "${env:userprofile}",
        ] {
            assert_eq!(
                expand_home_spellings(&format!(r"{spelling}\.ssh"), windows, r"C:\Users\alice"),
                r"C:\Users\alice\.ssh",
                "{spelling}"
            );
        }
        // In a POSIX shell `%USERPROFILE%` is an ordinary literal, and inventing a
        // home reference the shell will never make would misreport the target.
        assert_eq!(
            expand_home_spellings(
                "%USERPROFILE%/.ssh",
                Path::new("/home/alice"),
                "/home/alice"
            ),
            "%USERPROFILE%/.ssh"
        );
    }

    #[test]
    fn replacement_ignores_case_and_keeps_the_surrounding_text() {
        assert_eq!(
            replace_ignoring_case(r"X%UserProfile%Y%USERPROFILE%Z", "%userprofile%", "H"),
            "XHYHZ"
        );
        assert_eq!(
            replace_ignoring_case("nothing", "%userprofile%", "H"),
            "nothing"
        );
    }
}

#[cfg(test)]
mod wrapper_tests {
    use super::*;

    fn context() -> RiskContext {
        RiskContext {
            working_dir: Some(PathBuf::from("/work/project")),
            home_dir: Some(PathBuf::from("/home/alice")),
        }
    }

    fn verdict(command: &str) -> GateOutcome {
        let assessment =
            assess_command(command, ShellSyntax::Bash, &context()).expect("the command must parse");
        gate(&assessment)
    }

    /// The operand a wrapper reads before its program, when it takes one.
    fn sample_operand(wrapper: &str) -> Option<&'static str> {
        match wrapper {
            "chroot" => Some("/mnt"),
            "taskset" => Some("0x3"),
            "flock" => Some("/tmp/lock"),
            "chrt" => Some("99"),
            "timeout" => Some("5"),
            _ => None,
        }
    }

    /// Every way a wrapper can be written up to the word where its program begins:
    /// bare, with each option it knows (a value-taking one with its value as the next
    /// word, with a computed word in its value's slot, attached, and as `--name=value`),
    /// with `--`, each followed by the operand the wrapper takes. Options that introduce
    /// a script (`env -S`, `flock -c`) are covered by their own tests.
    fn prefixes(entry: &WrapperOptions) -> Vec<String> {
        let wrapper = entry.wrapper;
        let mut prefixes = vec![wrapper.to_owned()];
        for option in entry.value {
            if script_option(wrapper, option).is_some() {
                continue;
            }
            prefixes.push(format!("{wrapper} {option} sample"));
            // `$VAL` may be empty at runtime, and then `sample` is the value.
            prefixes.push(format!("{wrapper} {option} $VAL sample"));
            if option.starts_with("--") {
                prefixes.push(format!("{wrapper} {option}=sample"));
            } else {
                prefixes.push(format!("{wrapper} {option}sample"));
            }
        }
        for flag in entry.flag {
            prefixes.push(format!("{wrapper} {flag}"));
        }
        prefixes.push(format!("{wrapper} --"));
        prefixes
            .into_iter()
            .map(|prefix| match sample_operand(wrapper) {
                Some(operand) => format!("{prefix} {operand}"),
                None => prefix,
            })
            .collect()
    }

    #[test]
    fn every_wrapper_has_an_option_table_and_every_table_names_a_wrapper() {
        for wrapper in WRAPPER_COMMANDS {
            assert!(
                wrapper_options(wrapper).is_some(),
                "`{wrapper}` has no entry in WRAPPER_OPTIONS"
            );
        }
        for entry in WRAPPER_OPTIONS {
            assert!(
                WRAPPER_COMMANDS.contains(&entry.wrapper),
                "`{}` has an option table but is not a wrapper",
                entry.wrapper
            );
            for option in entry.value {
                assert!(
                    !entry.flag.contains(option),
                    "`{}` lists {option} as both value-taking and a flag",
                    entry.wrapper
                );
            }
        }
    }

    /// What the catastrophic table refuses, in each shape a wrapper hands on: a bare
    /// program, a shell running a script, that script with an expansion it never uses,
    /// and a program whose target is spelled `key=value`. `sh` and `dd` were durations
    /// to `timeout` — every letter of theirs is a unit suffix — and `dd of=/dev/sda` is
    /// the one refused program whose target is not a bare path.
    const DENIED_SUFFIXES: &[&str] = &[
        "rm -rf /",
        "sh -c 'rm -rf /'",
        "sh -c 'rm -rf / $UNUSED'",
        "dd of=/dev/sda",
    ];

    /// Every catastrophic line the enumeration judges: each prefix of [`prefixes`] in
    /// front of each of [`DENIED_SUFFIXES`] — bare, with a computed word before the
    /// program, and with a computed word and a value — then in front of a nested
    /// `sudo -Eu root`, then each suffix behind the options the tables do not know.
    fn denied_corpus() -> Vec<String> {
        let mut denied = Vec::new();
        for entry in WRAPPER_OPTIONS {
            let wrapper = entry.wrapper;
            let operand =
                sample_operand(wrapper).map_or_else(String::new, |operand| format!(" {operand}"));
            for prefix in prefixes(entry) {
                for suffix in DENIED_SUFFIXES {
                    denied.push(format!("{prefix} {suffix}"));
                    denied.push(format!("{prefix} $COMPUTED {suffix}"));
                    denied.push(format!("{prefix} $COMPUTED value {suffix}"));
                }
                denied.push(format!("{prefix} sudo -Eu root rm -rf /"));
            }
            for unknown in [
                "--zuno-unknown",
                "--zuno-unknown value",
                "-Z",
                "-Z value",
                "-$COMPUTED",
            ] {
                for suffix in DENIED_SUFFIXES {
                    denied.push(format!("{wrapper} {unknown}{operand} {suffix}"));
                }
            }
            // An unknown letter in front of a value-taking one: `-Zu root` is either
            // `-Z -u root` or `-Z` with the value `u` and `root` the program.
            if let Some(letter) = entry
                .value
                .iter()
                .find_map(|option| option.strip_prefix('-').filter(|rest| rest.len() == 1))
            {
                for suffix in DENIED_SUFFIXES {
                    denied.push(format!("{wrapper} -Z{letter} value{operand} {suffix}"));
                }
            }
        }
        denied
    }

    /// A benign program behind every prefix, so the tables do not trade denials for
    /// prompts.
    fn allowed_corpus() -> Vec<String> {
        WRAPPER_OPTIONS
            .iter()
            .flat_map(|entry| {
                prefixes(entry)
                    .into_iter()
                    .map(|prefix| format!("{prefix} ls -la"))
            })
            .collect()
    }

    /// A catastrophic program is denied wherever it can sit after a wrapper: directly,
    /// behind every option the table knows — with a static value and with a computed
    /// word in the value's slot — behind an option it does not know — read as a flag and
    /// as taking a value — and behind a word the shell computes. A benign program in the
    /// same places stays `Allow` behind every known option, so the tables do not trade
    /// denials for prompts. Every wrong verdict is collected so a failure names them all.
    #[test]
    fn a_catastrophic_program_is_denied_in_every_position_after_every_wrapper() {
        let denied = denied_corpus();
        let allowed = allowed_corpus();
        let checked = denied.len() + allowed.len();
        let mut failures = Vec::new();
        for command in &denied {
            let outcome = verdict(command);
            if !matches!(outcome, GateOutcome::Deny { .. }) {
                failures.push(format!(
                    "expected a denial for {command:?}, got {outcome:?}"
                ));
            }
        }
        for command in &allowed {
            let outcome = verdict(command);
            if outcome != GateOutcome::Allow {
                failures.push(format!("expected Allow for {command:?}, got {outcome:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {checked} wrapper lines got the wrong verdict:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// Lines the released gate allowed with no prompt although each runs `rm -rf /` or
    /// `dd of=/dev/sda` — every one was run with a fake `rm` first on `PATH`, and the
    /// fake was invoked with `-rf /`. The computed word sits in a slot the walk consumed
    /// without asking whether it may vanish: attached to its option (`-u$EMPTY`), as an
    /// operand (`chroot $ROOT`, `taskset $MASK`, `flock $L`), as a duration (`5$X`), as
    /// the script itself (`flock -c $X`), or `=`-joined (`--user=$VAL`, where real
    /// `sudo` happens to reject the empty user; the gate does not model that).
    const VANISHING_WORD_HOLES: &[&str] = &[
        "sudo -u$EMPTY root rm -rf /",
        "sudo -Eu$EMPTY root rm -rf /",
        "sudo -u$EMPTY root sh -c 'rm -rf /'",
        "sudo -u$EMPTY root dd of=/dev/sda",
        "sudo -u${EMPTY} root rm -rf /",
        "sudo -u$(true) root rm -rf /",
        "sudo -u`true` root rm -rf /",
        "sudo --user=$VAL root rm -rf /",
        "nice -n$N 5 rm -rf /",
        "env -u$EMPTY X rm -rf /",
        "env -u$EMPTY X -S 'rm -rf /'",
        "ionice -c$C 3 rm -rf /",
        "stdbuf -o$M L rm -rf /",
        "time -o$F out rm -rf /",
        "xargs -I$I {} rm -rf /",
        "timeout -s$S KILL 5 rm -rf /",
        "timeout 5$X rm -rf /",
        "flock -w$W 5 /tmp/lock rm -rf /",
        "exec -a$A foo rm -rf /",
        "sudo -C$N 3 rm -rf /",
        "sudo -g$G wheel rm -rf /",
        "env sudo -u$EMPTY root rm -rf /",
        "timeout 5 sudo -u$EMPTY root rm -rf /",
        "chroot $ROOT /mnt rm -rf /",
        "chroot --skip-chdir $R /mnt rm -rf /",
        "taskset $MASK 0x3 rm -rf /",
        "taskset -c $C 0-3 rm -rf /",
        "flock $L /tmp/lock rm -rf /",
        "flock -n $L /tmp/lock rm -rf /",
        "flock $L /tmp/lock -c 'rm -rf /'",
        "flock /tmp/lock -c $X 'rm -rf /'",
        "flock /tmp/lock -c$X 'rm -rf /'",
        "sudo taskset $MASK 0x3 rm -rf /",
        "nice taskset -c $C 0-3 rm -rf /",
    ];

    /// The same shapes around a benign program stay `Allow`: the fork rule reads every
    /// later word as a program, and `ls`, `-la`, `/mnt`, `cargo` and `make` are harmless
    /// ones.
    const VANISHING_WORD_BENIGN: &[&str] = &[
        "chroot $ROOT /mnt ls -la",
        "sudo -u$USER ls -la",
        "sudo -u $USER ls -la",
        "sudo -u root ls",
        "sudo --user=$USER ls",
        "nice -n$N make -j4",
        "env -u$EMPTY X make",
        "env FOO=bar make",
        "env FOO=$BAR make",
        "timeout 5 cargo test",
        "timeout inf cargo test",
        "timeout 5$X cargo test",
        "taskset $MASK 0x3 ls",
        "flock $L /tmp/lock ls",
    ];

    /// The script option in a spelling `script_option` did not recognise — clustered
    /// (`-iS`, `-nc`, `-lc`), attached (`-S'…'`, `-c'…'`) or `=`-joined
    /// (`--split-string=…`, `--command=…`) — so the script was an ordinary word and the
    /// line was `Allow` or a prompt. `env -iS 'rm -rf /'` was run live: `-i` clears
    /// `PATH`, so the real `rm` resolved and refused `/` itself.
    const CLUSTERED_SCRIPT_OPTION_HOLES: &[&str] = &[
        "env -iS 'rm -rf /'",
        "env -0S 'rm -rf /'",
        "env -vS 'rm -rf /'",
        "env -S'rm -rf /'",
        "env -iS'rm -rf /'",
        "env --split-string='rm -rf /'",
        "env -u $EMPTY X -S'rm -rf /'",
        "env -u $EMPTY X --split-string='rm -rf /'",
        "sudo env -S'rm -rf /'",
        "sudo env -iS 'rm -rf /'",
        "sudo env --split-string='rm -rf /'",
        "nice env --split-string='rm -rf /'",
        "timeout 5 env -S'rm -rf /'",
        "flock /tmp/lock -nc 'rm -rf /'",
        "flock /tmp/lock -c'rm -rf /'",
        "flock /tmp/lock --command='rm -rf /'",
        "sudo flock /tmp/lock -c'rm -rf /'",
        "su --command='rm -rf / $X'",
        "su --command='rm -rf /'",
        "su -c'rm -rf /'",
        "su -lc 'rm -rf /'",
        "su root -c'rm -rf /'",
    ];

    /// A `$'…'` script whose body holds an escape. The body was left as written, its `$`
    /// made the word merely computed, and the nested parse of the raw text found no
    /// `rm`: `sh -c $'echo hi\nrm -rf /'` was a prompt while `sh -c 'echo hi; rm -rf /'`
    /// was refused. Run live, the fake `rm` was invoked with `-rf /`.
    const ANSI_C_SCRIPT_HOLES: &[&str] = &[
        "sh -c $'echo hi\\nrm -rf /'",
        "sh -c $'rm\\x20-rf\\x20/'",
        "bash -c $'rm -rf \\x2f'",
        "sh -c $'rm -rf \\057'",
        "sh -c $'rm -rf /\\n'",
        "sudo sh -c $'echo hi\\nrm -rf /'",
        "sh -c $'echo $HOME\\nrm -rf /'",
        "eval $'echo hi\\nrm -rf /'",
        "su -c $'echo hi\\nrm -rf /'",
    ];

    /// The consumed word may vanish whatever slot the walk placed it in.
    #[test]
    fn a_computed_word_the_walk_consumes_in_any_slot_may_vanish() {
        let mut failures = Vec::new();
        for command in VANISHING_WORD_HOLES {
            let outcome = verdict(command);
            if !matches!(outcome, GateOutcome::Deny { .. }) {
                failures.push(format!(
                    "expected a denial for {command:?}, got {outcome:?}"
                ));
            }
        }
        for command in VANISHING_WORD_BENIGN {
            let outcome = verdict(command);
            if outcome != GateOutcome::Allow {
                failures.push(format!("expected Allow for {command:?}, got {outcome:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} wrong verdicts:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// A script option is the script option however it is spelled.
    #[test]
    fn a_clustered_attached_or_joined_script_option_still_introduces_the_script() {
        let mut failures = Vec::new();
        for command in CLUSTERED_SCRIPT_OPTION_HOLES {
            let outcome = verdict(command);
            if !matches!(outcome, GateOutcome::Deny { .. }) {
                failures.push(format!(
                    "expected a denial for {command:?}, got {outcome:?}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} wrong verdicts:\n{}",
            failures.len(),
            failures.join("\n")
        );
        // An environment query is still one: nothing after `env` can be a script.
        for command in ["env", "env -i", "env -0", "env FOO=bar", "env -u X -i"] {
            assert_eq!(verdict(command), GateOutcome::Allow, "{command:?}");
        }
        // The script option with no script runs nothing the gate can name.
        for command in ["env -S", "env -iS", "flock /tmp/lock -c"] {
            let outcome = verdict(command);
            assert!(
                matches!(outcome, GateOutcome::Confirm { .. }),
                "expected a confirmation for {command:?}, got {outcome:?}"
            );
        }
    }

    /// `$'…'` is read the way the shell reads it: escapes expanded, then parsed.
    #[test]
    fn an_ansi_c_quoted_script_is_read_after_its_escapes_are_expanded() {
        for (word, literal) in [
            ("$'echo hi\\nrm -rf /'", "echo hi\nrm -rf /"),
            ("$'a\\tb'", "a\tb"),
            ("$'it\\'s'", "it's"),
            ("$'\\x2f'", "/"),
            ("$'\\057'", "/"),
            ("$'a\\\\b'", "a\\b"),
            ("$'\\u002f'", "/"),
            ("$'\\q'", "\\q"),
            ("$'rm'", "rm"),
            ("r$'m'", "rm"),
            ("$'/'", "/"),
        ] {
            assert_eq!(
                static_shell_word(word, ShellSyntax::Bash),
                literal,
                "{word}"
            );
        }
        // An unterminated body is not an ANSI-C body at all: the `$` is an ordinary
        // character and the quote is the shell's, so the word stays computed.
        assert_eq!(static_shell_word("$'abc", ShellSyntax::Bash), "$abc");
        assert!(is_dynamic_word("$'abc", ShellSyntax::Bash));
        let mut failures = Vec::new();
        for command in ANSI_C_SCRIPT_HOLES {
            let outcome = verdict(command);
            if !matches!(outcome, GateOutcome::Deny { .. }) {
                failures.push(format!(
                    "expected a denial for {command:?}, got {outcome:?}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} wrong verdicts:\n{}",
            failures.len(),
            failures.join("\n")
        );
        // A genuine expansion inside the body is still reported beside the denial.
        let outcome = verdict("sh -c $'echo $HOME\\nrm -rf /'");
        assert!(
            matches!(outcome, GateOutcome::Deny { ref reason }
                if reason.contains("cannot be checked statically")
                    && reason.contains("protected system")),
            "{outcome:?}"
        );
        // A computed target inside the body is still only a prompt.
        let outcome = verdict("sh -c $'rm -rf $UNSET\\n'");
        assert!(
            matches!(outcome, GateOutcome::Confirm { .. }),
            "expected a confirmation, got {outcome:?}"
        );
        // A residual hole, reported rather than hidden: an ANSI-C escape can spell a
        // quote, and the materialised script then has quoting that does not close, so the
        // nested parser reads `rm -rf /` as part of one string and cannot judge it. The
        // line is held for a human instead of being allowed, which is what the expansion
        // gained: before it, the raw `$'…'` word was merely computed and the same prompt
        // was all the gate could say.
        let outcome = verdict("sh -c $'echo it\\'s fine\\nrm -rf /'");
        assert!(
            matches!(outcome, GateOutcome::Confirm { ref reason, .. }
                if reason.contains("cannot be checked statically")),
            "an unreadable script must be held, not allowed: {outcome:?}"
        );
    }

    /// Words the shell computes, in the spellings the walk must treat alike.
    const COMPUTED_FORMS: &[&str] = &["$X", "${X}", "$(true)", "`true`"];

    /// `Allow < Confirm < Deny`; a line the parser rejects is refused by the shell tool
    /// before anything runs, so it ranks above a denial.
    fn rank(command: &str) -> u8 {
        match assess_command(command, ShellSyntax::Bash, &context()) {
            Ok(assessment) => match gate(&assessment) {
                GateOutcome::Allow => 0,
                GateOutcome::Confirm { .. } => 1,
                GateOutcome::Deny { .. } => 2,
            },
            Err(_) => 3,
        }
    }

    /// The words of one simple command, split the way the production parser splits them.
    /// A `$(true)` inside a word is a command of its own to the parser; the outermost
    /// command is the one whose source is the whole line.
    fn words(command: &str) -> Vec<String> {
        let analysis = analyze_command(command, ShellSyntax::Bash).expect("the line must parse");
        analysis
            .commands
            .into_iter()
            .max_by_key(|resource| resource.source.len())
            .expect("one command")
            .tokens
    }

    /// Where the wrappers' own words end: the first word that is a destructive program,
    /// a shell, `su`, or a quoted script. A computed word attached to one of those is a
    /// computed program — `${X}rm` may run anything and the gate cannot know it is `rm`
    /// — so only the wrappers' options, values and operands before it are mutated.
    fn program_boundary(words: &[String]) -> usize {
        words
            .iter()
            .position(|token| {
                let word = static_shell_word(token, ShellSyntax::Bash);
                let program = command_name(token, ShellSyntax::Bash);
                word.contains(char::is_whitespace)
                    || is_destructive_command(&program)
                    || SHELL_COMMANDS.contains(&program.as_str())
                    || program == "su"
            })
            .unwrap_or(words.len())
    }

    /// A computed word glued to a wrapper's name is a computed program: `${X}env -S …`
    /// may be `env -S …` and the gate cannot know it, so the name itself is not mutated.
    fn is_wrapper_name(token: &str) -> bool {
        WRAPPER_COMMANDS.contains(&command_name(token, ShellSyntax::Bash).as_str())
    }

    /// Whether the word at `index` hands the next word to a shell as a script: a
    /// computed word in front of it (`${X}-S 'rm -rf /'`) is a computed option that may
    /// be the script option, which is the same limit as a computed program.
    fn introduces_script(words: &[String], index: usize) -> bool {
        static_shell_word(&words[index], ShellSyntax::Bash).starts_with('-')
            && words.get(index + 1).is_some_and(|next| {
                static_shell_word(next, ShellSyntax::Bash).contains(char::is_whitespace)
            })
    }

    /// The invariant every slot-specific fix so far was chasing, pinned once: a word the
    /// shell computes may vanish, so inserting one anywhere among the wrappers' words —
    /// as a word of its own, glued to the front of the next word, glued to the end of
    /// the previous one, in each spelling — can add a finding but never remove one. Every
    /// line of the enumeration and every line this round named is mutated at every
    /// position and the mutated verdict must not rank below the original.
    #[test]
    fn a_computed_word_anywhere_among_the_wrappers_words_never_lowers_the_verdict() {
        let mut lines = denied_corpus();
        for set in [
            VANISHING_WORD_HOLES,
            VANISHING_WORD_BENIGN,
            CLUSTERED_SCRIPT_OPTION_HOLES,
            ANSI_C_SCRIPT_HOLES,
        ] {
            lines.extend(set.iter().map(|line| (*line).to_owned()));
        }
        let mut mutations = 0usize;
        let mut judged = HashSet::new();
        let mut violations = Vec::new();
        for line in &lines {
            let original = rank(line);
            let words = words(line);
            let boundary = program_boundary(&words);
            let mut check = |mutated: Vec<String>| {
                let command = mutated.join(" ");
                if !judged.insert(command.clone()) {
                    return;
                }
                mutations += 1;
                let mutated = rank(&command);
                if mutated < original {
                    violations.push(format!(
                        "{command:?} ranks {mutated}, below {original} for {line:?}"
                    ));
                }
            };
            for form in COMPUTED_FORMS {
                for position in 0..=words.len() {
                    let mut separate = words.clone();
                    separate.insert(position, (*form).to_owned());
                    check(separate);
                }
                for index in 1..boundary {
                    if is_wrapper_name(&words[index]) {
                        continue;
                    }
                    let mut suffixed = words.clone();
                    suffixed[index] = format!("{}{form}", words[index]);
                    check(suffixed);
                    if introduces_script(&words, index) {
                        continue;
                    }
                    let mut prefixed = words.clone();
                    prefixed[index] = format!("{form}{}", words[index]);
                    check(prefixed);
                }
            }
        }
        println!(
            "metamorphic coverage: {} lines, {mutations} distinct mutations",
            lines.len()
        );
        assert!(
            violations.is_empty(),
            "{} of {mutations} mutations of {} lines lowered the verdict:\n{}",
            violations.len(),
            lines.len(),
            violations.join("\n")
        );
    }

    /// A computed word in the slot of an option the table knows takes a value may be
    /// empty at runtime. Then it vanishes, the word after it is the value and the
    /// program shifts one word right: `sudo -u $EMPTY root rm -rf /` runs `rm -rf /` as
    /// root, and reading `$EMPTY` as the value judged `root` and allowed the line with no
    /// prompt at all — a denial the released gate gave. Every later word is therefore
    /// also read as the program, as a program and not as more of the wrapper's options,
    /// so `-la` in `sudo -u $USER ls -la` is a program name and the line stays `Allow`.
    #[test]
    fn a_computed_option_value_may_vanish_so_every_later_word_is_read_as_the_program() {
        for command in [
            "sudo -u $EMPTY root rm -rf /",
            "sudo -u $EMPTY root dd of=/dev/sda",
            "sudo -u $EMPTY root sh -c 'rm -rf /'",
            "env sudo -u $EMPTY root rm -rf /",
            "watch -n $S 1 rm -rf /",
            "exec -a $A foo rm -rf /",
            "chroot --userspec $U root /mnt rm -rf /",
            "sudo --user $EMPTY root rm -rf /",
            "sudo -Eu $EMPTY root rm -rf /",
            "doas -u $EMPTY root rm -rf /",
            "nice -n $N 5 rm -rf /",
            "xargs -I $I {} rm -rf /",
            "stdbuf -o $M L rm -rf /",
            "ionice -c $C 3 rm -rf /",
            // The word after the vanished value can also hand the wrapper a script.
            "env -u $EMPTY X -S 'rm -rf /'",
            "flock -w $W 5 /tmp/lock -c 'rm -rf /'",
        ] {
            let outcome = verdict(command);
            assert!(
                matches!(outcome, GateOutcome::Deny { .. }),
                "expected a denial for {command:?}, got {outcome:?}"
            );
        }
        for command in [
            "sudo -u $USER ls -la",
            "sudo -u root ls",
            "nice -n $N make -j4",
            "timeout -k $K 5 cargo test",
            "chroot --userspec $U /mnt ls -la",
        ] {
            assert_eq!(verdict(command), GateOutcome::Allow, "{command:?}");
        }
        // With `USER` empty, `ls` is the user and `$DIR` is the program.
        let outcome = verdict("sudo -u $USER ls $DIR");
        assert!(
            matches!(outcome, GateOutcome::Confirm { ref reason, .. }
                if reason.contains("computed at runtime")),
            "a computed word after a computed value is a computed program: {outcome:?}"
        );
    }

    /// `timeout`'s duration is a number with an optional unit, or `inf`. Accepting any
    /// word spelled from digits and unit letters made `sh` and `dd` durations, so
    /// `timeout 5 sh -c 'rm -rf /'` read `-c` as a timeout option and
    /// `timeout 5 dd of=/dev/sda` judged `of=/dev/sda` as the program: both ran.
    #[test]
    fn a_timeout_duration_must_look_like_one() {
        for duration in [
            "5", "5s", "1.5m", "1d", "10h", ".5", "+5", "1e3", "0x10", "inf", "infinity", "INF",
        ] {
            assert!(is_timeout_duration(duration), "{duration:?} is a duration");
        }
        for word in [
            "sh", "dd", "s", "d", "", "-5", "cargo", "+", ".", "rm", "inff",
        ] {
            assert!(!is_timeout_duration(word), "{word:?} is not a duration");
        }
        for command in [
            "timeout 5 dd of=/dev/sda",
            "timeout -- 5 sh -c 'rm -rf /'",
            "timeout 5 sh -c 'rm -rf /'",
            "timeout 5 sh -c 'sudo -u root rm -rf /'",
            "timeout -k 5 10s dd of=/dev/sda",
            "timeout inf rm -rf /",
            "timeout +5 rm -rf /",
            "timeout 1e3 rm -rf /",
            "timeout .5 rm -rf /",
            "timeout 1d rm -rf /",
        ] {
            let outcome = verdict(command);
            assert!(
                matches!(outcome, GateOutcome::Deny { .. }),
                "expected a denial for {command:?}, got {outcome:?}"
            );
        }
        for command in [
            "timeout 5 cargo test",
            "timeout inf cargo test",
            "timeout infinity cargo test",
            "timeout -k 5 10s cargo test",
            "timeout .5 ls",
            "timeout 5 sh",
        ] {
            assert_eq!(verdict(command), GateOutcome::Allow, "{command:?}");
        }
        // The duration rule moved these off the denial list on purpose, and each was run
        // to check that nothing destructive happens. `timeout` needs a duration it
        // accepts, and a word that is not one is a usage error: `timeout '' rm -rf /`
        // exits 125 with `invalid time interval` and runs nothing at all (GNU coreutils
        // 9.4, measured). Where the word *is* a duration, the next word is the program
        // and `rm -rf /` is only its arguments: `timeout 5 sh rm -rf /` makes `sh` open a
        // script *file* named `rm` — exit 2, `cannot open rm` — and `timeout 5 s|m|ms
        // rm -rf /` exits 127 because no such program exists. None of them can run the
        // `rm` program, so none of them is a denial.
        for (command, reason) in [
            (
                "timeout 5 sh rm -rf /",
                "`sh rm -rf /` runs the script file `rm`, not the `rm` program: measured \
                 exit 2, `sh: 0: cannot open rm`",
            ),
            (
                "timeout '' rm -rf /",
                "an empty duration is a usage error: measured exit 125, `timeout: invalid \
                 time interval`, nothing runs",
            ),
            (
                "timeout \"\" rm -rf /",
                "the quoted empty duration is the same usage error: measured exit 125",
            ),
            (
                "timeout 5 s rm -rf /",
                "`s` is the program and `rm -rf /` its arguments: measured exit 127, no \
                 such command",
            ),
            (
                "timeout 5 . rm -rf /",
                "`.` is the program: measured exit 126, permission denied",
            ),
        ] {
            assert_eq!(
                verdict(command),
                GateOutcome::Allow,
                "{command:?}: {reason}"
            );
        }
        // `timeout 5 dd rm -rf /` does run `dd` — measured, with a fake `dd` on `PATH` —
        // so it is not an intended `Allow`; `dd` with no `of=` operand has no target the
        // gate can name, which is a confirmation and not a denial.
        let outcome = verdict("timeout 5 dd rm -rf /");
        assert!(
            matches!(outcome, GateOutcome::Confirm { ref reason, .. }
                if reason.contains("target could not be determined")),
            "`dd` runs here, with an unknown target: {outcome:?}"
        );
    }

    /// One expansion the script never even uses turned a permanent denial into a
    /// prompt: `sh -c 'rm -rf /'` was refused, `sh -c 'rm -rf / $UNUSED'` was
    /// confirmable, because a script with a `$` anywhere was abandoned instead of read.
    /// The script is now read; the computed word stays a finding, a static `rm -rf /`
    /// beside it stays a denial, and a computed target inside it is still only a prompt.
    #[test]
    fn an_unused_expansion_inside_a_script_cannot_downgrade_a_denial() {
        for command in [
            "sh -c 'rm -rf / $UNUSED'",
            "sh -c 'echo $PWD; rm -rf /'",
            "sh -c 'rm -rf $HOME'",
            "bash -c 'rm -rf ${HOME}'",
            "eval 'rm -rf $HOME'",
            "su -c 'rm -rf $HOME'",
            "env -S 'rm -rf / $UNUSED'",
            "sudo sh -c 'env $X rm -rf /'",
            "flock /tmp/lock -c 'rm -rf / $X'",
            "timeout 5 sh -c 'rm -rf / $UNUSED'",
            "sh -c 'sh -c \"rm -rf / $UNUSED\"'",
        ] {
            let outcome = verdict(command);
            assert!(
                matches!(outcome, GateOutcome::Deny { ref reason }
                    if reason.contains("cannot be checked statically")
                        && reason.contains("protected system")),
                "expected a denial that also reports the computed word for {command:?}, got {outcome:?}"
            );
        }
        for command in [
            "sh -c 'rm -rf $UNSET'",
            "sh -c 'rm -rf ./build $X'",
            "sh -c \"$SCRIPT\"",
            "sh -c 'echo $PWD'",
        ] {
            let outcome = verdict(command);
            assert!(
                matches!(outcome, GateOutcome::Confirm { .. }),
                "expected a confirmation for {command:?}, got {outcome:?}"
            );
        }
    }

    /// The primary reading other layers take is the one the gate always had: unknown
    /// options as flags, computed words as programs, and `xargs` still visible.
    #[test]
    fn the_primary_reading_is_the_first_one() {
        let tokens = |command: &str| {
            command
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let sudo = tokens("sudo --zuno-unknown value rm -rf /");
        let (command, wrapper) = unwrap_wrappers(&sudo, ShellSyntax::Bash);
        assert_eq!(command, &sudo[2..]);
        assert_eq!(wrapper.as_deref(), Some("sudo"));

        let computed = tokens("env $FOO rm -rf /");
        let (command, wrapper) = unwrap_wrappers(&computed, ShellSyntax::Bash);
        assert_eq!(command, &computed[1..]);
        assert_eq!(wrapper.as_deref(), Some("env"));
        assert_eq!(
            wrapper_readings(&computed, ShellSyntax::Bash).len(),
            7,
            "the computed word, then every later word as the program, and — because the \
             computed word may expand to `env -S` — every later word as a script"
        );

        let xargs = tokens("xargs -0 rm -rf");
        let (command, wrapper) = unwrap_wrappers(&xargs, ShellSyntax::Bash);
        assert_eq!(command, &xargs[2..]);
        assert_eq!(wrapper.as_deref(), Some("xargs"));
    }

    #[test]
    fn short_clusters_and_attached_values_are_read_per_option() {
        assert_eq!(wrapper_option_arity("sudo", "-Eu"), OptionArity::Value);
        assert_eq!(wrapper_option_arity("sudo", "-uroot"), OptionArity::Flag);
        assert_eq!(wrapper_option_arity("sudo", "-EH"), OptionArity::Flag);
        assert_eq!(wrapper_option_arity("sudo", "-Zu"), OptionArity::Unknown);
        assert_eq!(wrapper_option_arity("sudo", "-ZE"), OptionArity::Flag);
        assert_eq!(wrapper_option_arity("sudo", "-Z"), OptionArity::Unknown);
        assert_eq!(
            wrapper_option_arity("sudo", "--user=root"),
            OptionArity::Flag
        );
        assert_eq!(wrapper_option_arity("sudo", "--user"), OptionArity::Value);
        assert_eq!(
            wrapper_option_arity("sudo", "--preserve-env"),
            OptionArity::Flag
        );
        assert_eq!(wrapper_option_arity("sudo", "--zuno"), OptionArity::Unknown);
        assert_eq!(
            wrapper_option_arity("sudo", "-$FLAGS"),
            OptionArity::Dynamic
        );
        assert_eq!(
            wrapper_option_arity("sudo", "-E$FLAGS"),
            OptionArity::Dynamic
        );
        assert_eq!(wrapper_option_arity("sudo", "-u$USER"), OptionArity::Flag);
        assert_eq!(
            wrapper_option_arity("sudo", "--$NAME"),
            OptionArity::Dynamic
        );
        assert_eq!(
            wrapper_option_arity("sudo", "--user=$USER"),
            OptionArity::Flag
        );
        assert_eq!(wrapper_option_arity("nice", "-10"), OptionArity::Flag);
        assert_eq!(wrapper_option_arity("nice", "-n"), OptionArity::Value);
        assert_eq!(wrapper_option_arity("xargs", "-i"), OptionArity::Flag);
        assert_eq!(wrapper_option_arity("xargs", "-I"), OptionArity::Value);
        assert_eq!(wrapper_option_arity("stdbuf", "-oL"), OptionArity::Flag);
        assert_eq!(wrapper_option_arity("env", "-"), OptionArity::Flag);
    }
}

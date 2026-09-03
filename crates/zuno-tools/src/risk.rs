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

const WRAPPER_COMMANDS: &[&str] = &[
    "sudo", "doas", "env", "nice", "ionice", "time", "timeout", "nohup", "xargs", "command",
    "builtin", "exec", "setsid", "stdbuf", "chroot", "watch",
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
    if source_starts_with_dynamic_command(&resource.source) {
        findings.push(unknown_target_finding(
            "command name is computed at runtime, so destructive behavior cannot be checked"
                .to_owned(),
        ));
        return Ok(());
    }
    let Some(program) = tokens.first().map(|token| command_name(token, syntax)) else {
        return Ok(());
    };

    if program == "eval" {
        return assess_embedded_script(&tokens[1..], syntax, context, depth, "eval", findings);
    }
    if SHELL_COMMANDS.contains(&program.as_str()) {
        return assess_shell_script(tokens, syntax, context, depth, &program, findings);
    }
    if program == "su" {
        return assess_su_script(tokens, syntax, context, depth, findings);
    }
    if let Some(script) = env_split_script(tokens, syntax) {
        return assess_embedded_script(&script, syntax, context, depth, "env -S", findings);
    }
    if env_without_child_command(tokens, syntax) {
        return Ok(());
    }

    let (tokens, wrapper) = unwrap_wrappers(tokens, syntax);
    let Some(program) = tokens.first().map(|token| command_name(token, syntax)) else {
        if let Some(wrapper) = wrapper {
            findings.push(unknown_target_finding(format!(
                "`{wrapper}` runs a command that could not be identified statically"
            )));
        }
        return Ok(());
    };

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

    let mut targets = destructive_targets(tokens, &program);
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
        "commit" if has_git_option(args, "--amend", None) => {
            assess_local_git_history_rewrite(
                repository_override,
                "`git commit --amend` replaces the current commit",
                findings,
            );
            true
        }
        "rebase" if !is_rebase_recovery(args) => {
            assess_local_git_history_rewrite(
                repository_override,
                "`git rebase` rewrites local commit history",
                findings,
            );
            true
        }
        "tag" if has_git_option(args, "--force", Some('f')) => {
            assess_local_git_history_rewrite(
                repository_override,
                "`git tag --force` moves an existing tag",
                findings,
            );
            true
        }
        "push" => assess_git_push(args, findings),
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
/// every check keyed on a subcommand. Global options are skipped the way git parses
/// them, so the subcommand is the first token that is not one.
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
        let token = unquote(raw);
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
    tokens
        .get(index)
        .map(|token| (unquote(token).to_ascii_lowercase(), &tokens[index + 1..]))
}

fn has_git_option(args: &[String], long: &str, short: Option<char>) -> bool {
    args.iter().map(|token| unquote(token)).any(|token| {
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

fn is_rebase_recovery(args: &[String]) -> bool {
    [
        "--abort",
        "--continue",
        "--skip",
        "--quit",
        "--show-current-patch",
    ]
    .iter()
    .any(|option| has_git_option(args, option, None))
}

fn assess_git_push(args: &[String], findings: &mut Vec<RiskFinding>) -> bool {
    let rendered = args.iter().map(|token| unquote(token)).collect::<Vec<_>>();
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

fn env_without_child_command(tokens: &[String], syntax: ShellSyntax) -> bool {
    if tokens
        .first()
        .is_none_or(|token| command_name(token, syntax) != "env")
    {
        return false;
    }

    let mut index = 1;
    let mut options = true;
    while let Some(token) = tokens.get(index) {
        let token = unquote(token);
        if options && token == "--" {
            options = false;
            index += 1;
            continue;
        }
        if options && matches!(token.as_str(), "-S" | "--split-string") {
            return false;
        }
        if options && token.starts_with("--split-string=") {
            return false;
        }
        if options && token.starts_with('-') {
            index += 1;
            if wrapper_option_consumes_value(Some("env"), &token) && tokens.get(index).is_some() {
                index += 1;
            }
            continue;
        }
        if token.contains('=') {
            index += 1;
            continue;
        }
        return false;
    }
    true
}

fn env_split_script(tokens: &[String], syntax: ShellSyntax) -> Option<Vec<String>> {
    if tokens
        .first()
        .is_none_or(|token| command_name(token, syntax) != "env")
    {
        return None;
    }

    let mut index = 1;
    while let Some(token) = tokens.get(index) {
        let token = unquote(token);
        let split = match token.as_str() {
            "-S" | "--split-string" => tokens.get(index + 1).map(|value| (value.clone(), 2)),
            _ => token
                .strip_prefix("--split-string=")
                .or_else(|| token.strip_prefix("-S").filter(|value| !value.is_empty()))
                .map(|value| (value.to_owned(), 1)),
        };
        if let Some((script, consumed)) = split {
            let mut embedded = vec![script];
            embedded.extend_from_slice(&tokens[index + consumed..]);
            return Some(embedded);
        }
        if token == "--" {
            return None;
        }
        if token.starts_with('-') {
            index += 1;
            if wrapper_option_consumes_value(Some("env"), &token) && tokens.get(index).is_some() {
                index += 1;
            }
            continue;
        }
        if token.contains('=') {
            index += 1;
            continue;
        }
        return None;
    }
    None
}

fn assess_shell_script(
    tokens: &[String],
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
    program: &str,
    findings: &mut Vec<RiskFinding>,
) -> Result<(), ToolError> {
    let script = tokens
        .iter()
        .position(|token| is_command_script_option(&unquote(token)))
        .and_then(|index| tokens.get(index + 1));
    let Some(script) = script else {
        return Ok(());
    };
    assess_embedded_script(
        std::slice::from_ref(script),
        syntax,
        context,
        depth,
        program,
        findings,
    )
}

fn assess_su_script(
    tokens: &[String],
    syntax: ShellSyntax,
    context: &RiskContext,
    depth: usize,
    findings: &mut Vec<RiskFinding>,
) -> Result<(), ToolError> {
    let script = tokens
        .iter()
        .position(|token| matches!(unquote(token).as_str(), "-c" | "--command"))
        .and_then(|index| tokens.get(index + 1));
    let Some(script) = script else {
        findings.push(unknown_target_finding(
            "`su` changes identity and its executed command could not be checked statically"
                .to_owned(),
        ));
        return Ok(());
    };
    assess_embedded_script(
        std::slice::from_ref(script),
        syntax,
        context,
        depth,
        "su",
        findings,
    )
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
    let script = tokens
        .iter()
        .map(|token| unquote(token))
        .collect::<Vec<_>>()
        .join(" ");
    if script.is_empty() || is_dynamic_path(&script) || depth >= MAX_EMBEDDED_SCRIPT_DEPTH {
        findings.push(unknown_target_finding(format!(
            "`{runner}` runs a command whose destructive target cannot be checked statically"
        )));
        return Ok(());
    }
    let embedded = assess_command_at_depth(&script, syntax, context, depth + 1)?;
    if embedded.findings.is_empty() {
        return Ok(());
    }
    findings.extend(embedded.findings);
    Ok(())
}

/// Strip `sudo`, `env VAR=x`, `timeout 5`, `xargs` and the other wrappers from the
/// front of a command, returning what they run and the innermost wrapper's name.
///
/// Shared with [`crate::navigation`] so `env FOO=1 rg x` is `rg` under both gates;
/// two wrapper tables would drift, and a wrapper only one of them knew would let a
/// command through the other.
pub(crate) fn unwrap_wrappers(
    tokens: &[String],
    syntax: ShellSyntax,
) -> (&[String], Option<String>) {
    let mut remaining = tokens;
    let mut last_wrapper = None;
    loop {
        let Some(program) = remaining.first().map(|token| command_name(token, syntax)) else {
            return (remaining, last_wrapper);
        };
        if !WRAPPER_COMMANDS.contains(&program.as_str()) {
            return (remaining, last_wrapper);
        }
        last_wrapper = Some(program);
        remaining = &remaining[1..];
        let mut consumed = 0;
        while let Some(token) = remaining.get(consumed) {
            let token = unquote(token);
            if token == "--" {
                consumed += 1;
                break;
            }
            if token.starts_with('-') {
                consumed += 1;
                if wrapper_option_consumes_value(last_wrapper.as_deref(), &token)
                    && remaining.get(consumed).is_some()
                {
                    consumed += 1;
                }
                continue;
            }
            if last_wrapper.as_deref() == Some("env") && token.contains('=') {
                consumed += 1;
                continue;
            }
            if last_wrapper.as_deref() == Some("timeout")
                && token.chars().all(|character| {
                    character.is_ascii_digit() || matches!(character, '.' | 's' | 'm' | 'h' | 'd')
                })
            {
                consumed += 1;
                continue;
            }
            break;
        }
        remaining = &remaining[consumed..];
        if last_wrapper.as_deref() == Some("chroot") && !remaining.is_empty() {
            remaining = &remaining[1..];
        }
    }
}

fn wrapper_option_consumes_value(wrapper: Option<&str>, option: &str) -> bool {
    matches!(
        (wrapper, option),
        (
            Some("sudo"),
            "-u" | "--user"
                | "-g"
                | "--group"
                | "-h"
                | "--host"
                | "-p"
                | "--prompt"
                | "-C"
                | "--close-from"
        ) | (Some("doas"), "-u")
            | (
                Some("env"),
                "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string"
            )
            | (Some("nice"), "-n" | "--adjustment")
            | (
                Some("ionice"),
                "-c" | "--class" | "-n" | "--classdata" | "-t" | "--ignore"
            )
            | (Some("timeout"), "-k" | "--kill-after" | "-s" | "--signal")
            | (
                Some("xargs"),
                "-a" | "--arg-file"
                    | "-E"
                    | "--eof"
                    | "-I"
                    | "--replace"
                    | "-L"
                    | "--max-lines"
                    | "-n"
                    | "--max-args"
                    | "-P"
                    | "--max-procs"
                    | "-s"
                    | "--max-chars"
            )
            | (
                Some("stdbuf"),
                "-i" | "--input" | "-o" | "--output" | "-e" | "--error"
            )
            | (Some("chroot"), "--userspec" | "--groups")
    )
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

fn destructive_targets(tokens: &[String], program: &str) -> Vec<String> {
    if program == "dd" {
        return tokens
            .iter()
            .skip(1)
            .filter_map(|token| unquote(token).strip_prefix("of=").map(str::to_owned))
            .collect();
    }

    let mut targets = Vec::new();
    let mut options_done = false;
    let mut skip_value = false;
    for token in tokens.iter().skip(1) {
        let token = unquote(token);
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
    reduce_shell_word(text, Some(shell_escape(syntax)))
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

fn source_starts_with_dynamic_command(source: &str) -> bool {
    let Some(first) = source.split_ascii_whitespace().next() else {
        return false;
    };
    let first = unquote(first);
    first.starts_with('$') || first.starts_with('`') || first.starts_with("$(")
}

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

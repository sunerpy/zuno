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
/// [`RiskContext::from_env`] snapshots only `HOME`; redirect assessment may still
/// inspect one fully resolved static target to distinguish creation from replacement.
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
            home_dir: std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from),
        }
    }
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
        let Some(program) = resource.tokens.first().map(|token| command_name(token)) else {
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
                        .and_then(|target| resolve_directory_target(&target, &context))
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
                            resolve_directory_target(&target, &context)
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
                        resolve_directory_target(&target, &context)
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

fn resolve_directory_target(raw: &str, context: &RiskContext) -> Option<PathBuf> {
    if matches!(static_brace_expansions(raw), BraceExpansions::Unknown)
        || (is_dynamic_path(raw) && !home_is_fully_expanded(raw, context))
    {
        return None;
    }
    let expanded = expand_path(raw, context);
    expanded.is_absolute().then(|| normalize_path(&expanded))
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
        assess_redirect_target(&redirect, context, findings);
    }

    let tokens = &resource.tokens;
    if source_starts_with_dynamic_command(&resource.source) {
        findings.push(unknown_target_finding(
            "command name is computed at runtime, so destructive behavior cannot be checked"
                .to_owned(),
        ));
        return Ok(());
    }
    let Some(program) = tokens.first().map(|token| command_name(token)) else {
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
    if let Some(script) = env_split_script(tokens) {
        return assess_embedded_script(&script, syntax, context, depth, "env -S", findings);
    }
    if env_without_child_command(tokens) {
        return Ok(());
    }

    let (tokens, wrapper) = unwrap_wrappers(tokens);
    let Some(program) = tokens.first().map(|token| command_name(token)) else {
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
    if program == "git" && assess_git(&resource.source, tokens, context, findings) {
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
        assess_destructive_target(&target, context, absent_temp_file_cleanup, findings);
    }
    Ok(())
}

fn assess_git(
    source: &str,
    tokens: &[String],
    context: &RiskContext,
    findings: &mut Vec<RiskFinding>,
) -> bool {
    let Some((subcommand, args)) = git_subcommand(tokens) else {
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

fn git_uses_repository_override(tokens: &[String]) -> bool {
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

fn git_subcommand(tokens: &[String]) -> Option<(String, &[String])> {
    if tokens
        .first()
        .is_none_or(|token| command_name(token) != "git")
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

fn env_without_child_command(tokens: &[String]) -> bool {
    if tokens
        .first()
        .is_none_or(|token| command_name(token) != "env")
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

fn env_split_script(tokens: &[String]) -> Option<Vec<String>> {
    if tokens
        .first()
        .is_none_or(|token| command_name(token) != "env")
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
pub(crate) fn unwrap_wrappers(tokens: &[String]) -> (&[String], Option<String>) {
    let mut remaining = tokens;
    let mut last_wrapper = None;
    loop {
        let Some(program) = remaining.first().map(|token| command_name(token)) else {
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
        assess_destructive_target(&root, context, false, findings);
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
    absent_temp_file_cleanup: bool,
    findings: &mut Vec<RiskFinding>,
) {
    match static_brace_expansions(raw) {
        BraceExpansions::Expanded(targets) => {
            for target in targets {
                assess_destructive_target(&target, context, absent_temp_file_cleanup, findings);
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
    let expanded = expand_path(raw, context);
    if context.working_dir.is_none() && !expanded.is_absolute() {
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

fn assess_redirect_target(raw: &str, context: &RiskContext, findings: &mut Vec<RiskFinding>) {
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
    let expanded = expand_path(&unquoted, context);
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

fn expand_path(raw: &str, context: &RiskContext) -> PathBuf {
    let raw = static_shell_word(raw);
    let expanded_home = context.home_dir.as_ref().map(|home| {
        let home = home.to_string_lossy();
        raw.replace("${HOME}", &home).replace("$HOME", &home)
    });
    let mut text = expanded_home.unwrap_or(raw);
    if let Some(home) = &context.home_dir {
        if text == "~" {
            text = home.to_string_lossy().into_owned();
        } else if let Some(rest) = text.strip_prefix("~/") {
            text = home.join(rest).to_string_lossy().into_owned();
        }
    }
    let path = PathBuf::from(text);
    if path.is_absolute() {
        normalize_path(&path)
    } else if let Some(cwd) = &context.working_dir {
        normalize_path(&cwd.join(path))
    } else {
        normalize_path(&path)
    }
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

fn is_catastrophic_target(path: &Path, context: &RiskContext) -> bool {
    let path = normalize_path(path);
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
    if context.home_dir.is_none() {
        return false;
    }
    let without_home = raw.replace("${HOME}", "").replace("$HOME", "");
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
    path.contains('*') || path.contains('?')
}

fn is_destructive_command(program: &str) -> bool {
    DESTRUCTIVE_COMMANDS.contains(&program) || program.starts_with("mkfs.")
}

/// The lowercase file name of a command token with shell quoting removed, so
/// `"/usr/bin/RG"` names the same program as `rg`.
pub(crate) fn command_name(token: &str) -> String {
    let static_name = static_shell_word(token);
    Path::new(&static_name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&static_name)
        .to_ascii_lowercase()
}

fn static_shell_word(text: &str) -> String {
    let mut word = String::with_capacity(text.len());
    let mut quote = None;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
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
    if escaped {
        word.push('\\');
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

//! Mechanical enforcement of "query the CodeGraph index before you grep".
//!
//! A repository that carries a `.codegraph/` index has already paid for the answer
//! to "where is this and who calls it". The project's instructions tell the model
//! to ask that index before it reads or greps source, but an instruction is
//! prompt-level: a model can simply not follow it, and one did — it grepped its way
//! to a wrong conclusion while the correct answer sat in the index. This module
//! turns the instruction into a check the host performs on every tool call, so the
//! failure is caught at the call rather than in the conclusion.
//!
//! [`RepositoryNavigationPolicy`] is a session-scoped decision object and nothing
//! else: no filesystem writes, no database, no logging. Its one filesystem question
//! — is there an index? — is answered by the host through [`index_present`], so the
//! policy itself is pure and every rule below is testable with a string and a JSON
//! value. The host owns the mode, the wiring into the dispatch hook, and the
//! persistence of what the policy decides.
//!
//! # What is governed
//!
//! Source *navigation*: the native `grep` and `glob` tools, and a `shell` command
//! whose program is `rg`, `grep`, `ag`, `find`, `sed`, or `awk` (or `git grep`)
//! reading the tree. `read` of a specific path is not navigation — an agent that
//! already knows the file is not searching for it — and neither is a write (`sed
//! -i`, `find -delete`), a pipeline stage that filters another command's output
//! (`cargo test | grep FAILED` is verification, not navigation), or a command that
//! merely mentions one of those words as an argument. Shell classification
//! consumes [`crate::shell::analyze_command`]'s tree-sitter resources rather than
//! matching the command line as a string, so `cd crates && rg foo`, a pipeline, a
//! subshell, a `$(...)` substitution, an `env`-prefixed invocation and a
//! `bash -c '...'` script are all seen for what they run. The `execute` batch tool
//! runs its sub-calls without passing back through the dispatch hook, so a batch is
//! judged by its sub-calls in the order they were listed.
//!
//! # The rule
//!
//! When the repository is indexed, the first navigation of the session must be a
//! CodeGraph call. Once one has been observed, everything is allowed: the policy's
//! job is ordering, not prohibition. [`NavigationMode::Strict`] refuses a
//! navigation that comes first; [`NavigationMode::Advise`] lets it through, reports
//! it once, and is then quiet for the rest of the session, because a session nagged
//! on every call learns to ignore the nag. [`NavigationMode::Off`] — the default
//! everywhere — imposes nothing, so installing the policy changes no behaviour until
//! a user turns it on.
//!
//! # Why `codegraph status` counts as a query
//!
//! The instructions make `codegraph status . --json` the first CodeGraph step, and
//! this policy wants it before any query: a query issued without it is reported as
//! [`INDEX_UNCHECKED`] (once, never refused, because refusing the very query the
//! policy exists to obtain would push the model back toward grep). `status` also
//! releases the gate on its own, for two reasons. A model that ran it has engaged
//! with the index rather than bypassed it, which is the behaviour the policy exists
//! to obtain. More importantly, the policy never sees tool output: `status` may have
//! reported the index stale, corrupt, or mismatched with the worktree, in which case
//! grep *is* the correct next move, and a policy that kept refusing it would trap
//! the session behind an index that cannot answer. Refusing to release on `status`
//! would turn CodeGraph's own recovery guidance into a dead end.
//!
//! # Why decisions are returned rather than queued
//!
//! [`RepositoryNavigationPolicy::observe`] returns the decision and keeps no log of
//! it. A durable event needs the session and call identifiers, and those meet the
//! decision only in the host's hook, which has them in hand at the moment it calls
//! `observe`; a drain queue would either lose that correlation or force the
//! identifiers into a signature that has no other use for them. Returning the
//! decision also leaves the policy with no buffer a host can forget to drain.

use crate::risk::{
    SHELL_COMMANDS, command_name, is_command_script_option, unquote, unwrap_wrappers,
};
use crate::shell::{CommandResource, ShellSyntax, analyze_command};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

/// The directory whose presence at a worktree root marks an indexed repository.
pub const INDEX_DIRECTORY: &str = ".codegraph";

/// Stable code for a source-navigation call made before any CodeGraph query.
///
/// Carried by both the refusal `Strict` issues and the notice `Advise` issues, so a
/// durable event names the condition and the decision variant names the handling.
pub const INDEX_BYPASSED: &str = "navigation.index_bypassed";

/// Stable code for a CodeGraph query issued before `codegraph status` checked the index.
pub const INDEX_UNCHECKED: &str = "navigation.index_unchecked";

/// How far a `bash -c '...'` or `execute` nesting is followed before giving up.
///
/// The same bound [`crate::risk`] uses: deep enough for every real invocation, and a
/// stop for a pathological one.
const MAX_NESTING_DEPTH: usize = 4;

/// The concrete remedy every non-`Allow` decision names, so the model is told what
/// to do rather than only what it did wrong.
const REMEDY: &str = "Run `codegraph status . --json` to check the index, then \
                      `codegraph explore \"<question>\" -p .` or `codegraph search \
                      \"<symbol>\" -p .` (or the equivalent CodeGraph MCP tool).";

const CODEGRAPH_PROGRAM: &str = "codegraph";
/// Runners that fetch and execute a package's binary, so `npx codegraph status` is
/// still the CodeGraph executable.
const PACKAGE_RUNNERS: &[&str] = &["npx", "bunx", "pnpx"];
/// CodeGraph subcommands that manage the index rather than ask it anything. They
/// neither satisfy the policy nor violate it.
const LIFECYCLE_SUBCOMMANDS: &[&str] = &[
    "init", "sync", "index", "reindex", "clean", "daemon", "watch", "help", "version",
];
/// Programs that search the tree when given a pattern and not fed from a pipe.
const SEARCH_PROGRAMS: &[&str] = &["rg", "grep", "egrep", "fgrep", "ag"];
/// Stream editors that read source when given a file and not fed from a pipe.
const FILTER_PROGRAMS: &[&str] = &["sed", "awk", "gawk", "mawk"];
const NATIVE_NAVIGATION_TOOLS: &[&str] = &["grep", "glob"];
const SHELL_TOOL: &str = "shell";
const EXECUTE_TOOL: &str = "execute";

/// How much the policy imposes. `Off` is the default so that an installed policy
/// changes nothing until a user turns it on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavigationMode {
    /// Every call is allowed and nothing is recorded.
    #[default]
    Off,
    /// The first violation is reported and allowed; the session is then left alone.
    Advise,
    /// A navigation before any CodeGraph query fails until one has run.
    Strict,
}

impl NavigationMode {
    /// The configuration spelling, so a durable event can name the mode in force.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Advise => "advise",
            Self::Strict => "strict",
        }
    }
}

/// What the host must do with one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationDecision {
    /// Let the call run and record nothing.
    Allow,
    /// Let the call run, but report `detail` under the stable `code`.
    Advise { code: &'static str, detail: String },
    /// The call must fail; `detail` is the message the model reads instead of a result.
    Refuse { code: &'static str, detail: String },
}

impl NavigationDecision {
    /// Whether the call proceeds with nothing to report.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The stable code of a decision that is not `Allow`.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Allow => None,
            Self::Advise { code, .. } | Self::Refuse { code, .. } => Some(code),
        }
    }

    /// The sentence telling the model what to do instead, for a decision that is not `Allow`.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::Advise { detail, .. } | Self::Refuse { detail, .. } => Some(detail),
        }
    }
}

/// Where one session stands with respect to the index.
#[derive(Debug, Clone, Copy, Default)]
struct State {
    /// A CodeGraph call has been observed, or `Advise` has already reported once;
    /// either way, navigation is now allowed without comment.
    satisfied: bool,
    /// `codegraph status` (or the status MCP tool) has been observed.
    status_checked: bool,
    /// Any CodeGraph call has been observed, which decides whether a query still
    /// earns the one-time "check the index first" notice.
    codegraph_seen: bool,
}

/// The session-scoped gate. One instance per session; share it behind an `Arc`.
///
/// Interior mutability keeps [`Self::observe`] callable from a `&self` hook while the
/// session's progress is tracked; a poisoned lock is recovered rather than
/// propagated because the state is three booleans that no panic can leave torn.
#[derive(Debug)]
pub struct RepositoryNavigationPolicy {
    mode: NavigationMode,
    indexed: bool,
    syntax: ShellSyntax,
    state: Mutex<State>,
}

impl RepositoryNavigationPolicy {
    /// A policy in `mode` for a repository whose index presence the host has
    /// established, normally through [`index_present`].
    ///
    /// Shell commands are read as Bash until [`Self::with_shell_syntax`] says otherwise.
    #[must_use]
    pub fn new(mode: NavigationMode, indexed: bool) -> Self {
        Self {
            mode,
            indexed,
            syntax: ShellSyntax::Bash,
            state: Mutex::new(State::default()),
        }
    }

    /// Read shell commands with `syntax`, for a host whose configured shell is PowerShell.
    #[must_use]
    pub fn with_shell_syntax(mut self, syntax: ShellSyntax) -> Self {
        self.syntax = syntax;
        self
    }

    /// The mode this policy was built with.
    #[must_use]
    pub const fn mode(&self) -> NavigationMode {
        self.mode
    }

    /// Whether the host reported an index; without one the policy always allows.
    #[must_use]
    pub const fn indexed(&self) -> bool {
        self.indexed
    }

    /// Whether the session has satisfied the policy, so navigation is now free.
    ///
    /// False for the whole session under `Off` or without an index, because those
    /// policies record nothing.
    #[must_use]
    pub fn satisfied(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .satisfied
    }

    /// Decide one tool call from its wire id and its arguments before it runs.
    ///
    /// A refused call does not advance the session: nothing in it will run, so a
    /// `codegraph status` listed after an `rg` in the same shell command or batch
    /// does not count as having happened. A call that proceeds advances the
    /// session by every constituent it runs, and when more than one constituent
    /// earns a notice only the first is returned, so a host reports one decision
    /// per call.
    pub fn observe(&self, tool: &str, args: &Value) -> NavigationDecision {
        if self.mode == NavigationMode::Off || !self.indexed {
            return NavigationDecision::Allow;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.satisfied && state.codegraph_seen {
            return NavigationDecision::Allow;
        }
        let mut scratch = *state;
        let mut decision = NavigationDecision::Allow;
        for observation in observations(tool, args, self.syntax, 0) {
            let next = self.decide(&mut scratch, observation, tool);
            if matches!(next, NavigationDecision::Refuse { .. }) {
                return next;
            }
            if decision.is_allow() {
                decision = next;
            }
        }
        *state = scratch;
        decision
    }

    fn decide(
        &self,
        state: &mut State,
        observation: Observation,
        tool: &str,
    ) -> NavigationDecision {
        match observation {
            Observation::Other => NavigationDecision::Allow,
            Observation::IndexCheck => {
                state.status_checked = true;
                state.codegraph_seen = true;
                state.satisfied = true;
                NavigationDecision::Allow
            }
            Observation::IndexQuery => {
                let unchecked = !state.status_checked && !state.codegraph_seen;
                state.codegraph_seen = true;
                state.satisfied = true;
                if unchecked {
                    NavigationDecision::Advise {
                        code: INDEX_UNCHECKED,
                        detail: format!(
                            "The `{tool}` tool queried the CodeGraph index before `codegraph \
                             status . --json` confirmed it is usable; the query proceeds, but \
                             run the status check first so a stale or missing index is noticed \
                             before its answers are trusted."
                        ),
                    }
                } else {
                    NavigationDecision::Allow
                }
            }
            Observation::Navigation(subject) => {
                if state.satisfied {
                    return NavigationDecision::Allow;
                }
                match self.mode {
                    NavigationMode::Off => NavigationDecision::Allow,
                    NavigationMode::Strict => NavigationDecision::Refuse {
                        code: INDEX_BYPASSED,
                        detail: format!(
                            "{subject} before any CodeGraph query in this session, and this \
                             repository has a CodeGraph index that must be consulted first. \
                             {REMEDY} Once one CodeGraph query has run, this call is allowed."
                        ),
                    },
                    NavigationMode::Advise => {
                        state.satisfied = true;
                        NavigationDecision::Advise {
                            code: INDEX_BYPASSED,
                            detail: format!(
                                "{subject} before any CodeGraph query in this session, and this \
                                 repository has a CodeGraph index that should have been consulted \
                                 first; the call proceeds. Next time: {REMEDY} This notice is \
                                 issued once per session."
                            ),
                        }
                    }
                }
            }
        }
    }
}

/// Whether `worktree` carries a CodeGraph index at its root.
///
/// This is the one filesystem question the policy needs, kept out of
/// [`RepositoryNavigationPolicy`] so the policy stays pure. It looks at exactly one
/// root and does not walk upward: a linked git worktree may keep its index beside
/// the main checkout, and which root to ask is the host's call, because walking up
/// would also make every subdirectory of an unindexed project inherit whatever index
/// a parent happens to have.
#[must_use]
pub fn index_present(worktree: &Path) -> bool {
    worktree.join(INDEX_DIRECTORY).is_dir()
}

/// What one tool call, or one constituent of it, means to the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observation {
    /// A tool this policy has no opinion about.
    Other,
    /// `codegraph status` or the status MCP tool.
    IndexCheck,
    /// Any other CodeGraph question.
    IndexQuery,
    /// Source navigation, described for the decision's message.
    Navigation(String),
}

/// Every observation one call produces, in the order its constituents would run.
fn observations(tool: &str, args: &Value, syntax: ShellSyntax, depth: usize) -> Vec<Observation> {
    // An MCP tool is namespaced `{server}_{tool}`, so a CodeGraph server's tools read
    // `codegraph_codegraph_explore`; the substring test covers that and any other
    // spelling a server chooses.
    let lowered = tool.to_ascii_lowercase();
    if lowered.contains(CODEGRAPH_PROGRAM) {
        return vec![if lowered.contains("status") {
            Observation::IndexCheck
        } else {
            Observation::IndexQuery
        }];
    }
    if NATIVE_NAVIGATION_TOOLS.contains(&tool) {
        return vec![Observation::Navigation(format!(
            "The `{tool}` tool was called"
        ))];
    }
    if tool == SHELL_TOOL {
        // A missing or non-string command is the shell tool's argument error to report,
        // not a navigation decision.
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return vec![Observation::Other];
        };
        return shell_observations(command, syntax, depth);
    }
    if tool == EXECUTE_TOOL && depth < MAX_NESTING_DEPTH {
        let Some(calls) = args.get("tool_calls").and_then(Value::as_array) else {
            return vec![Observation::Other];
        };
        // A sub-call flattens its arguments beside `tool` and `intent`, so the
        // sub-call object is also its argument object.
        return calls
            .iter()
            .flat_map(|call| match call.get("tool").and_then(Value::as_str) {
                Some(subtool) => observations(subtool, call, syntax, depth + 1),
                None => vec![Observation::Other],
            })
            .collect();
    }
    vec![Observation::Other]
}

fn shell_observations(command: &str, syntax: ShellSyntax, depth: usize) -> Vec<Observation> {
    // The parser fails only when tree-sitter itself cannot produce a tree. A command
    // the policy cannot read is left to the shell tool's own gates rather than
    // refused on a guess.
    let Ok(analysis) = analyze_command(command, syntax) else {
        return vec![Observation::Other];
    };
    analysis
        .commands
        .iter()
        .flat_map(|resource| resource_observations(resource, syntax, depth))
        .collect()
}

fn resource_observations(
    resource: &CommandResource,
    syntax: ShellSyntax,
    depth: usize,
) -> Vec<Observation> {
    let (tokens, _) = unwrap_wrappers(&resource.tokens, syntax);
    let Some(program) = tokens.first().map(|token| program_name(token, syntax)) else {
        return vec![Observation::Other];
    };
    let arguments = &tokens[1..];
    let wrappers = &resource.tokens[..resource.tokens.len() - tokens.len()];
    // `xargs` turns the pipe into arguments, so `git ls-files | xargs grep foo` still
    // reads the files it names rather than filtering the previous stage's text.
    let fed_by_pipe = resource.stdin_from_pipeline
        && !wrappers
            .iter()
            .any(|token| command_name(token, syntax) == "xargs");

    if program == "eval" {
        let script = arguments
            .iter()
            .map(|token| unquote(token))
            .collect::<Vec<_>>()
            .join(" ");
        return nested_script(&script, syntax, depth);
    }
    if SHELL_COMMANDS.contains(&program.as_str()) {
        let script = arguments
            .iter()
            .position(|token| is_command_script_option(&unquote(token)))
            .and_then(|index| arguments.get(index + 1));
        return match script {
            Some(script) => nested_script(&unquote(script), syntax, depth),
            None => vec![Observation::Other],
        };
    }
    if let Some(observation) = codegraph_observation(&program, arguments, syntax) {
        return vec![observation];
    }
    if let Some(observation) = navigation_observation(&program, arguments, fed_by_pipe) {
        return vec![observation];
    }
    vec![Observation::Other]
}

fn nested_script(script: &str, syntax: ShellSyntax, depth: usize) -> Vec<Observation> {
    if script.trim().is_empty() || depth >= MAX_NESTING_DEPTH {
        return vec![Observation::Other];
    }
    shell_observations(script, syntax, depth + 1)
}

/// The CodeGraph executable, invoked directly or through a package runner.
fn codegraph_observation(
    program: &str,
    arguments: &[String],
    syntax: ShellSyntax,
) -> Option<Observation> {
    let arguments = if program == CODEGRAPH_PROGRAM {
        arguments
    } else if PACKAGE_RUNNERS.contains(&program) {
        let index = arguments
            .iter()
            .position(|token| !unquote(token).starts_with('-'))?;
        if program_name(&arguments[index], syntax) != CODEGRAPH_PROGRAM {
            return None;
        }
        &arguments[index + 1..]
    } else {
        return None;
    };
    // `codegraph --help` and a bare `codegraph` print usage; neither asks the index.
    let subcommand = arguments
        .iter()
        .map(|token| unquote(token).to_ascii_lowercase())
        .find(|token| !token.starts_with('-'))?;
    Some(match subcommand.as_str() {
        "status" => Observation::IndexCheck,
        lifecycle if LIFECYCLE_SUBCOMMANDS.contains(&lifecycle) => Observation::Other,
        _ => Observation::IndexQuery,
    })
}

/// A program reading the tree, or `None` for anything the policy leaves alone.
fn navigation_observation(
    program: &str,
    arguments: &[String],
    fed_by_pipe: bool,
) -> Option<Observation> {
    let navigation =
        |what: &str| Observation::Navigation(format!("The `shell` tool invoked `{what}`"));
    if program == "find" {
        // `find -delete` is a mutation; the policy does not govern writes.
        let deletes = arguments.iter().any(|token| unquote(token) == "-delete");
        return (!deletes).then(|| navigation("find"));
    }
    if program == "git" {
        return (git_subcommand(arguments).as_deref() == Some("grep"))
            .then(|| navigation("git grep"));
    }
    // A search program with nothing to search for prints usage — `command -v rg`,
    // a bare `grep` — and one fed by a pipe filters another command's output; neither
    // reads source.
    if arguments.is_empty() || fed_by_pipe {
        return None;
    }
    if SEARCH_PROGRAMS.contains(&program) {
        return Some(navigation(program));
    }
    if FILTER_PROGRAMS.contains(&program) {
        if program == "sed" && sed_edits_in_place(arguments) {
            return None;
        }
        return Some(navigation(program));
    }
    None
}

/// The first non-option word after `git`, skipping the global options that take a value.
fn git_subcommand(arguments: &[String]) -> Option<String> {
    let mut skip_value = false;
    for token in arguments.iter().map(|token| unquote(token)) {
        if skip_value {
            skip_value = false;
            continue;
        }
        if matches!(
            token.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            skip_value = true;
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        return Some(token.to_ascii_lowercase());
    }
    None
}

/// GNU `-i[SUFFIX]`/`--in-place` and BSD `-I`, alone or in a cluster such as `-ni`.
fn sed_edits_in_place(arguments: &[String]) -> bool {
    arguments.iter().map(|token| unquote(token)).any(|token| {
        if let Some(long) = token.strip_prefix("--") {
            return long == "in-place" || long.starts_with("in-place=");
        }
        token
            .strip_prefix('-')
            .is_some_and(|flags| flags.chars().any(|flag| matches!(flag, 'i' | 'I')))
    })
}

/// [`command_name`], which already drops a Windows program suffix.
fn program_name(token: &str, syntax: ShellSyntax) -> String {
    command_name(token, syntax)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strict() -> RepositoryNavigationPolicy {
        RepositoryNavigationPolicy::new(NavigationMode::Strict, true)
    }

    fn advise() -> RepositoryNavigationPolicy {
        RepositoryNavigationPolicy::new(NavigationMode::Advise, true)
    }

    fn shell(command: &str) -> Value {
        json!({ "command": command })
    }

    fn grep() -> Value {
        json!({ "pattern": "fn observe", "include": "*.rs" })
    }

    fn glob() -> Value {
        json!({ "pattern": "crates/**/*.rs" })
    }

    /// The detail of a decision that must be a refusal.
    fn refusal(decision: NavigationDecision) -> String {
        match decision {
            NavigationDecision::Refuse { code, detail } => {
                assert_eq!(code, INDEX_BYPASSED);
                detail
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The detail of refusing `command` under a fresh strict policy, naming the command
    /// when it was not refused.
    fn refused_command(command: &str) -> String {
        match strict().observe("shell", &shell(command)) {
            NavigationDecision::Refuse { code, detail } => {
                assert_eq!(code, INDEX_BYPASSED, "{command}");
                detail
            }
            other => panic!("expected `{command}` to be refused, got {other:?}"),
        }
    }

    /// The detail of a decision that must be a notice under `code`.
    fn notice(decision: NavigationDecision, expected: &'static str) -> String {
        match decision {
            NavigationDecision::Advise { code, detail } => {
                assert_eq!(code, expected);
                detail
            }
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn mode_off_allows_a_grep_with_and_without_an_index_and_records_nothing() {
        for indexed in [false, true] {
            let policy = RepositoryNavigationPolicy::new(NavigationMode::Off, indexed);
            assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
            assert_eq!(
                policy.observe("shell", &shell("rg foo")),
                NavigationDecision::Allow
            );
            assert!(!policy.satisfied(), "an inert policy tracks nothing");
        }
    }

    #[test]
    fn an_unindexed_repository_allows_every_navigation_tool_in_strict_mode() {
        let policy = RepositoryNavigationPolicy::new(NavigationMode::Strict, false);
        assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
        assert_eq!(policy.observe("glob", &glob()), NavigationDecision::Allow);
        assert_eq!(
            policy.observe("shell", &shell("find . -name '*.rs'")),
            NavigationDecision::Allow
        );
        assert!(!policy.satisfied());
    }

    #[test]
    fn the_first_grep_in_strict_mode_is_refused_with_the_codegraph_remedy() {
        let detail = refusal(strict().observe("grep", &grep()));
        assert!(detail.contains("The `grep` tool was called"), "{detail}");
        assert!(detail.contains("`codegraph status . --json`"), "{detail}");
        assert!(
            detail.contains("`codegraph explore \"<question>\" -p .`"),
            "{detail}"
        );
    }

    #[test]
    fn the_same_grep_is_allowed_once_a_codegraph_explore_has_run() {
        let policy = strict();
        refusal(policy.observe("grep", &grep()));
        // An MCP tool is namespaced `{server}_{tool}`.
        let decision = policy.observe(
            "codegraph_codegraph_explore",
            &json!({ "query": "where is observe" }),
        );
        assert!(!matches!(decision, NavigationDecision::Refuse { .. }));
        assert!(policy.satisfied());
        assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
        assert_eq!(
            policy.observe("shell", &shell("rg foo")),
            NavigationDecision::Allow
        );
    }

    #[test]
    fn a_directory_change_followed_by_rg_is_recognised_as_navigation() {
        let detail = refused_command("cd crates && rg foo");
        assert!(detail.contains("The `shell` tool invoked `rg`"), "{detail}");
    }

    #[test]
    fn a_shell_codegraph_search_satisfies_the_policy() {
        let policy = strict();
        let decision = policy.observe("shell", &shell("codegraph search \"observe\" -p ."));
        assert!(!matches!(decision, NavigationDecision::Refuse { .. }));
        assert!(policy.satisfied());
        assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
    }

    #[test]
    fn rg_named_as_an_argument_to_another_command_is_not_navigation() {
        for command in [
            "echo rg",
            "which rg grep",
            "command -v rg",
            "git commit -m 'prefer rg over grep'",
            "cargo install ripgrep grep",
            "rg",
            "ls crates/zuno-tools/src",
        ] {
            assert_eq!(
                strict().observe("shell", &shell(command)),
                NavigationDecision::Allow,
                "{command}"
            );
        }
    }

    #[test]
    fn advise_mode_reports_the_first_violation_once_and_then_stays_quiet() {
        let policy = advise();
        let detail = notice(policy.observe("grep", &grep()), INDEX_BYPASSED);
        assert!(detail.contains("the call proceeds"), "{detail}");
        assert!(detail.contains("`codegraph status . --json`"), "{detail}");
        assert!(policy.satisfied());
        assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
        assert_eq!(
            policy.observe("shell", &shell("rg foo")),
            NavigationDecision::Allow
        );
        assert_eq!(policy.observe("glob", &glob()), NavigationDecision::Allow);
    }

    #[test]
    fn index_present_reports_only_a_codegraph_directory_at_the_worktree_root() {
        let root = tempfile::tempdir().expect("temporary worktree");
        assert!(!index_present(root.path()));

        std::fs::write(root.path().join(INDEX_DIRECTORY), b"not a directory").expect("file");
        assert!(!index_present(root.path()), "a stray file is not an index");
        std::fs::remove_file(root.path().join(INDEX_DIRECTORY)).expect("remove file");

        std::fs::create_dir(root.path().join(INDEX_DIRECTORY)).expect("index directory");
        assert!(index_present(root.path()));
        let nested = root.path().join("crates").join("zuno-tools");
        std::fs::create_dir_all(&nested).expect("nested directory");
        assert!(!index_present(&nested), "the helper does not walk upward");
    }

    #[test]
    fn codegraph_status_alone_releases_the_gate() {
        let policy = strict();
        assert_eq!(
            policy.observe("shell", &shell("codegraph status . --json")),
            NavigationDecision::Allow
        );
        assert!(policy.satisfied());
        assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
        // A query after status earns no "check the index first" notice.
        assert_eq!(
            policy.observe(
                "shell",
                &shell("codegraph explore \"how does observe work\" -p .")
            ),
            NavigationDecision::Allow
        );
    }

    #[test]
    fn a_query_before_the_status_check_is_advised_once_and_never_refused() {
        for policy in [strict(), advise()] {
            let detail = notice(
                policy.observe("shell", &shell("codegraph explore \"observe\" -p .")),
                INDEX_UNCHECKED,
            );
            assert!(detail.contains("`codegraph status . --json`"), "{detail}");
            assert!(detail.contains("the query proceeds"), "{detail}");
            assert_eq!(
                policy.observe("shell", &shell("codegraph node \"observe\" -p .")),
                NavigationDecision::Allow,
                "the notice is issued once"
            );
            assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
        }
    }

    #[test]
    fn the_status_mcp_tool_counts_as_the_index_check() {
        let policy = strict();
        assert_eq!(
            policy.observe("codegraph_codegraph_status", &json!({})),
            NavigationDecision::Allow
        );
        assert_eq!(
            policy.observe("codegraph_codegraph_explore", &json!({ "query": "x" })),
            NavigationDecision::Allow
        );
        assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
    }

    #[test]
    fn a_pipeline_stage_filtering_another_commands_output_is_not_navigation() {
        for command in [
            "cargo test -p zuno-tools 2>&1 | grep -c FAILED",
            "ps aux | rg zuno",
            "git log --oneline | sed -n '1,5p'",
            "cargo clippy 2>&1 | awk '/warning/ {print}'",
            "cat Cargo.toml | grep version | head -1",
        ] {
            assert_eq!(
                strict().observe("shell", &shell(command)),
                NavigationDecision::Allow,
                "{command}"
            );
        }
        // The head of a pipeline still reads the tree.
        refused_command("rg foo 2>/dev/null | head -5");
    }

    #[test]
    fn xargs_feeding_grep_from_a_pipe_is_still_navigation() {
        let detail = refused_command("git ls-files | xargs grep -n foo");
        assert!(detail.contains("`grep`"), "{detail}");
    }

    #[test]
    fn a_wrapped_or_env_prefixed_rg_is_still_navigation() {
        for command in [
            "env RIPGREP_CONFIG_PATH=/dev/null rg foo",
            "timeout 30 rg foo crates",
            "nice -n 10 /usr/bin/rg foo",
            "sudo grep -rn foo /etc/zuno",
            "rg.exe foo",
        ] {
            refused_command(command);
        }
    }

    #[test]
    fn sed_in_place_is_a_write_but_sed_reading_a_file_is_navigation() {
        for command in [
            "sed -i 's/a/b/' crates/zuno-tools/src/lib.rs",
            "sed -ni 's/a/b/p' crates/zuno-tools/src/lib.rs",
            "sed --in-place=.bak 's/a/b/' src/lib.rs",
            "sed -I '' 's/a/b/' src/lib.rs",
        ] {
            assert_eq!(
                strict().observe("shell", &shell(command)),
                NavigationDecision::Allow,
                "{command}"
            );
        }
        for command in [
            "sed -n '100,200p' crates/zuno-tools/src/shell.rs",
            "awk '/fn observe/ {print FILENAME\":\"NR}' crates/zuno-tools/src/*.rs",
        ] {
            refused_command(command);
        }
    }

    #[test]
    fn the_read_tool_and_file_mutations_are_never_governed() {
        let policy = strict();
        for (tool, args) in [
            (
                "read",
                json!({ "file_path": "crates/zuno-tools/src/shell.rs" }),
            ),
            ("write", json!({ "file_path": "notes.md", "content": "x" })),
            (
                "edit",
                json!({ "file_path": "a.rs", "old_string": "a", "new_string": "b" }),
            ),
            ("apply_patch", json!({ "patch": "*** Begin Patch" })),
            ("shell", shell("cargo test -p zuno-tools -j 8")),
            ("shell", shell("git status --short")),
            ("web_search", json!({ "queries": ["rg"] })),
        ] {
            assert_eq!(
                policy.observe(tool, &args),
                NavigationDecision::Allow,
                "{tool}"
            );
        }
        assert!(
            !policy.satisfied(),
            "nothing here counted as a CodeGraph call either"
        );
    }

    #[test]
    fn a_nested_shell_script_is_classified_by_what_it_runs() {
        for command in [
            "bash -c 'rg foo'",
            "sh -c \"cd crates && grep -rn foo .\"",
            "eval \"rg foo\"",
            "bash -lc 'rg foo | head'",
        ] {
            refused_command(command);
        }
        let policy = strict();
        assert_eq!(
            policy.observe("shell", &shell("bash -c 'codegraph status . --json'")),
            NavigationDecision::Allow
        );
        assert!(policy.satisfied());
    }

    #[test]
    fn a_shell_call_without_a_command_string_is_left_to_the_shell_tool() {
        let policy = strict();
        assert_eq!(
            policy.observe("shell", &json!({})),
            NavigationDecision::Allow
        );
        assert_eq!(
            policy.observe("shell", &json!({ "command": 42 })),
            NavigationDecision::Allow
        );
        assert_eq!(
            policy.observe("shell", &Value::Null),
            NavigationDecision::Allow
        );
    }

    #[test]
    fn strict_mode_keeps_refusing_until_codegraph_has_run() {
        let policy = strict();
        refusal(policy.observe("grep", &grep()));
        refusal(policy.observe("shell", &shell("rg foo")));
        refusal(policy.observe("glob", &glob()));
        assert!(
            !policy.satisfied(),
            "a refusal does not advance the session"
        );
        assert_eq!(
            policy.observe("shell", &shell("codegraph status . --json")),
            NavigationDecision::Allow
        );
        assert_eq!(policy.observe("grep", &grep()), NavigationDecision::Allow);
    }

    #[test]
    fn a_package_runner_invoking_codegraph_counts_as_codegraph() {
        let policy = strict();
        assert_eq!(
            policy.observe("shell", &shell("npx -y codegraph status . --json")),
            NavigationDecision::Allow
        );
        assert!(policy.satisfied());
        // A runner invoking something else is not CodeGraph.
        let other = strict();
        assert_eq!(
            other.observe("shell", &shell("npx -y prettier --check .")),
            NavigationDecision::Allow
        );
        assert!(!other.satisfied());
    }

    #[test]
    fn codegraph_lifecycle_commands_neither_satisfy_nor_violate() {
        let policy = strict();
        for command in [
            "codegraph sync .",
            "codegraph init .",
            "codegraph --help",
            "codegraph",
        ] {
            assert_eq!(
                policy.observe("shell", &shell(command)),
                NavigationDecision::Allow,
                "{command}"
            );
        }
        assert!(!policy.satisfied());
        refusal(policy.observe("grep", &grep()));
    }

    #[test]
    fn git_grep_and_find_are_navigation_but_find_delete_is_a_mutation() {
        refused_command("git grep -n 'fn observe'");
        refused_command("git -C crates grep -n observe");
        refused_command("find . -name '*.rs' -newer Cargo.toml");
        refused_command("find");
        for command in [
            "find target -name '*.tmp' -delete",
            "git -C crates status",
            "git log --grep=observe",
        ] {
            assert_eq!(
                strict().observe("shell", &shell(command)),
                NavigationDecision::Allow,
                "{command}"
            );
        }
    }

    #[test]
    fn the_glob_tool_is_governed_like_grep() {
        let policy = strict();
        let detail = refusal(policy.observe("glob", &glob()));
        assert!(detail.contains("The `glob` tool was called"), "{detail}");
        policy.observe("codegraph_codegraph_status", &json!({}));
        assert_eq!(policy.observe("glob", &glob()), NavigationDecision::Allow);
    }

    #[test]
    fn a_command_substitution_and_a_subshell_running_rg_are_navigation() {
        for command in [
            "files=$(rg -l foo)",
            "(cd crates; rg foo)",
            "if rg -q foo; then echo found; fi",
            "for f in $(find . -name '*.rs'); do echo \"$f\"; done",
        ] {
            refused_command(command);
        }
    }

    #[test]
    fn a_shell_command_is_judged_by_the_order_its_constituents_run() {
        let policy = strict();
        assert_eq!(
            policy.observe("shell", &shell("codegraph status . --json && rg foo")),
            NavigationDecision::Allow
        );
        let reversed = strict();
        refusal(reversed.observe("shell", &shell("rg foo && codegraph status . --json")));
        assert!(
            !reversed.satisfied(),
            "a status that never ran, because the call was refused, does not count"
        );
    }

    #[test]
    fn an_execute_batch_is_judged_by_its_subcalls_in_order() {
        let ordered = json!({ "tool_calls": [
            { "tool": "codegraph_codegraph_status", "intent": "check the index" },
            { "tool": "grep", "intent": "find observe", "pattern": "fn observe" },
        ] });
        let policy = strict();
        assert_eq!(
            policy.observe("execute", &ordered),
            NavigationDecision::Allow
        );
        assert!(policy.satisfied());

        let reversed = json!({ "tool_calls": [
            { "tool": "shell", "intent": "search", "command": "rg foo" },
            { "tool": "codegraph_codegraph_status", "intent": "check the index" },
        ] });
        let policy = strict();
        refusal(policy.observe("execute", &reversed));
        assert!(!policy.satisfied());
        refusal(policy.observe("grep", &grep()));

        let benign = json!({ "tool_calls": [
            { "tool": "read", "intent": "read", "file_path": "Cargo.toml" },
        ] });
        assert_eq!(
            strict().observe("execute", &benign),
            NavigationDecision::Allow
        );
    }

    #[test]
    fn a_powershell_pipeline_filter_is_not_navigation_but_a_chained_rg_is() {
        let policy = strict().with_shell_syntax(ShellSyntax::PowerShell);
        assert_eq!(
            policy.observe("shell", &shell("cargo test 2>&1 | grep -c FAILED")),
            NavigationDecision::Allow
        );
        assert_eq!(
            policy.observe("shell", &shell("Get-Content Cargo.toml | rg version")),
            NavigationDecision::Allow,
            "a filter over a known file's content is not navigation"
        );
        refusal(policy.observe("shell", &shell("Set-Location crates && rg foo")));
        refusal(policy.observe("shell", &shell("rg foo | Select-Object -First 5")));
        refusal(policy.observe("shell", &shell("rg foo")));
    }

    #[test]
    fn navigation_mode_parses_its_lowercase_spelling_and_defaults_to_off() {
        assert_eq!(NavigationMode::default(), NavigationMode::Off);
        for (text, mode) in [
            ("\"off\"", NavigationMode::Off),
            ("\"advise\"", NavigationMode::Advise),
            ("\"strict\"", NavigationMode::Strict),
        ] {
            let parsed: NavigationMode = serde_json::from_str(text).expect(text);
            assert_eq!(parsed, mode);
            assert_eq!(serde_json::to_string(&mode).expect("serialize"), text);
            assert_eq!(format!("\"{}\"", mode.as_str()), text);
        }
        assert!(serde_json::from_str::<NavigationMode>("\"on\"").is_err());
    }

    #[test]
    fn a_decision_exposes_its_code_and_detail_for_the_durable_event() {
        assert_eq!(NavigationDecision::Allow.code(), None);
        assert_eq!(NavigationDecision::Allow.detail(), None);
        assert!(NavigationDecision::Allow.is_allow());
        let refused = strict().observe("grep", &grep());
        assert!(!refused.is_allow());
        assert_eq!(refused.code(), Some(INDEX_BYPASSED));
        assert!(
            refused
                .detail()
                .is_some_and(|detail| detail.contains("codegraph"))
        );
    }
}

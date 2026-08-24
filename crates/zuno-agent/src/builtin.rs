//! The native delegation roster: `build`, six specialists, and engine internals.
//!
//! # Why this roster is bounded on purpose
//!
//! A delegation surface fails in two directions. Too few agents and every task
//! funnels through one context window; too many and the primary agent spends its
//! turn choosing a lane instead of doing work. Each entry must justify what the
//! caller loses if it is absent. `deep` is the deliberate seventh lane: unlike the
//! bounded `worker`, it owns ambiguous cross-cutting implementation that needs a
//! sustained research-to-verification pass.
//!
//! The mapping from the OMO references: `oracle` and `observer`'s advisory half
//! collapse into [`ADVISOR`]; `fixer` becomes [`WORKER`] **without its amnesia**;
//! `designer` is dropped (this is not a UI-first tool, and its 0.7 temperature was
//! the only thing the roster spent taste on); `council`/`councillor` are dropped —
//! slim's own note prices multi-model consensus at "3x slower … 3x or more cost"
//! (`src/agents/orchestrator.ts:95`) and the advisor owns explicit alternatives.
//!
//! # Why every agent carries a *negative* boundary
//!
//! A positive description ("delegate when you need X") is what a model already
//! infers from a name. What it cannot infer is when *not* to delegate, and that is
//! the expensive mistake: a round trip through a child session to read one file
//! whose path the caller already had. Slim's `AGENT_DESCRIPTIONS`
//! (`src/agents/orchestrator.ts:41-113`) pairs every agent with a
//! "**Don't delegate when:**" clause for exactly this reason. Here the boundary is
//! a required field rather than a paragraph convention, so
//! [`tests`] can assert its presence instead of a reviewer noticing its absence.
//!
//! # Natural output by default
//!
//! User-facing agents answer in ordinary Markdown. Earlier drafts required XML-like
//! envelopes even though no runtime consumer parsed them; that added tokens and leaked
//! orchestration markup into final answers. Structured output belongs only where a typed
//! consumer exists. The three engine internals remain prompt-owned because their raw
//! completions are consumed directly.
//!
//! # No model ids
//!
//! Nothing here names a model. Every agent inherits the session model until a
//! preset or a per-agent override says otherwise, which is todo 64's job. The
//! inversion is slim's (`src/config/constants.ts:26-41`, every default
//! `undefined`, "so agents follow the global/session model"), and it is the single
//! most valuable thing to copy: the alternative — a built-in
//! `AGENT_MODEL_REQUIREMENTS` table like the parent's at `dist/index.js:24475` —
//! bakes today's model market into the binary and rots on every release.
//! [`tests`] scans every rendered string for model-shaped tokens so this stays
//! true by test rather than by intent.

use zuno_config::schema::agent::AgentMode;
use zuno_config::schema::permission::PermissionAction;
use zuno_llm::catalog::resolved::ModelCapabilities;
use zuno_permission::Rule;

/// The tool ids this roster's permission sets are allowed to name.
///
/// These are the model-facing wire ids from `zuno-tools/src/registry.rs:52-68`,
/// minus the three that a permission set cannot usefully name:
///
/// * `write` and `apply_patch` both route through the `edit` permission key
///   ([`zuno_permission::visibility::permission_key`]), so a rule keyed on either
///   name never matches and is dead config. Grant or deny `edit`.
/// * `invalid` is the registry's placeholder for a tool that failed to load; it is
///   never a policy target.
///
/// It is stated here rather than imported because the dependency edge runs the
/// other way — see the note in this crate's `Cargo.toml`. The cross-crate
/// assertion that these ids still match the registry belongs in `zuno-tools`, where
/// todo 65's `task` tool already sees both crates.
pub const GOVERNED_TOOL_IDS: [&str; 17] = [
    "bash",
    "read",
    "glob",
    "grep",
    "edit",
    "task",
    "webfetch",
    "plan_get",
    "plan_update",
    "todo_get",
    "todo_update",
    "web_search",
    "skill",
    "execute",
    "lsp",
    "question",
    "plan_exit",
];

/// Where an agent sits in the delegation graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The user-facing primary agent. The only role that may delegate.
    Orchestrator,
    /// Reachable through `task`, and therefore needs a delegation boundary.
    Subagent,
    /// Driven by the engine, never by a `task` call. See [`Boundary::NotDelegable`].
    Internal,
}

/// Whether the agent may spawn children.
///
/// The distinction is load-bearing rather than cosmetic: an agent that can
/// delegate can also fan out recursively, which is why todo 65 gates `task` on
/// depth. Exactly one entry in the roster is [`Self::MayDelegate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delegation {
    /// May call `task`.
    MayDelegate,
    /// May not call `task`. Naming the right specialist in prose is still fine —
    /// that is advice to the caller, not a child session.
    NoChildren,
}

/// Whether the agent may modify the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// Inspection only.
    ReadOnly,
    /// May edit, and therefore also runs verification.
    Capable,
}

/// Whether the agent may gather context it was not handed.
///
/// This exists as a field because of a specific defect worth naming. Slim's
/// `fixer` is deliberately amnesiac — "NO external research", "No multi-step
/// research/planning" (`omo-slim`) — so any
/// *explore → decide → implement → verify* task has to bounce back through the
/// orchestrator between every phase, and the orchestrator's context absorbs all of
/// it. [`WORKER`] is [`Self::Allowed`] instead; that is deviation (1) of the three
/// recorded in the design notes. It keeps the property that actually
/// bounds a worker — [`Delegation::NoChildren`] — and drops the one that only
/// bounded its usefulness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Research {
    /// May search, read, fetch, and iterate until the task is done.
    Allowed,
    /// Confined to the tools its permission set grants, with no research lane.
    Confined,
}

/// What must be true of the resolved catalog for the agent to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Always present.
    Always,
    /// Present exactly when some resolved model accepts image input.
    ///
    /// Deviation (2) in the design notes: slim disables its
    /// multimodal agent by default (`src/config/constants.ts:91`). An opt-in
    /// context-hygiene feature is one nobody opts into, and the cost of the agent
    /// existing is a paragraph in a prompt. So the gate is a capability question,
    /// not a preference.
    VisionModel,
}

/// Whether extension-supplied tools reach this agent.
///
/// A `'*': 'deny'` base has one consequence worth stating out loud: MCP and plugin
/// tools arrive with server-derived ids (`zuno-mcp/src/stdio.rs:984` builds
/// `{server}_{tool}`) that no static allow-list can name, so under
/// [`zuno_permission::visibility::is_tool_hidden`] they are invisible to every agent
/// whose last matching rule is the wildcard deny. Blinding the *primary* agent to
/// a server the user deliberately configured would be a regression, so the
/// orchestrator declares [`Self::Inherit`] and
/// [`Agent::rules_with_extension_tools`] names those ids explicitly once the
/// registry has assembled them. Deny-by-default is preserved: a tool is still
/// reachable only by being named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionTools {
    /// Extension tool ids are appended as allows once known.
    Inherit,
    /// Extension tools stay hidden. A bounded lane should not grow new
    /// capabilities because the user installed an unrelated server.
    Excluded,
}

/// The agent's delegation boundary, or the reason it has none.
///
/// The variants are the table's exemption mechanism. A newly added agent cannot
/// quietly skip its boundary: it must either supply one, or claim
/// [`Self::NotDelegable`] — which [`tests`] only accepts for [`Role::Internal`]
/// entries whose names appear in [`INTERNAL_NAMES`], a list fixed by the upstream
/// engine rather than by this roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// The "Don't delegate when…" clause, without its label.
    DontDelegateWhen(&'static str),
    /// The agent is not a `task` target, so there is no delegation decision to
    /// bound. The reason is required so the exemption stays an argument.
    NotDelegable {
        /// Why no caller ever chooses this agent.
        reason: &'static str,
    },
}

impl Boundary {
    /// The clause text, when there is one.
    #[must_use]
    pub const fn clause(&self) -> Option<&'static str> {
        match self {
            Self::DontDelegateWhen(text) => Some(text),
            Self::NotDelegable { .. } => None,
        }
    }

    /// Rendered for the orchestrator's prompt and for `agent list`.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::DontDelegateWhen(text) => format!("**Don't delegate when:** {text}"),
            Self::NotDelegable { reason } => {
                format!("**Not delegable:** {reason}")
            }
        }
    }
}

/// What consumes an agent's reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputContract {
    /// User-facing Markdown with no harness-only wrapper.
    Natural,
    /// The engine consumes the raw completion — a title string, a compacted
    /// transcript, a session summary. Wrapping those in tags would mean stripping
    /// the tags again at the only call site.
    EnginePrompt {
        /// The upstream prompt constant that specifies the format instead.
        prompt: &'static str,
    },
}

/// A deny-by-default permission set.
///
/// Ported from `omo-slim`, including its
/// apparent redundancy: a `'*'` catch-all deny **and** explicit denies **and**
/// explicit allows. The redundancy is the point — an explicit deny survives a
/// future change to how the catch-all is interpreted, and it makes the boundary
/// legible in a rendered ruleset instead of implied by an absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Denied by name on top of the catch-all.
    pub denied: &'static [&'static str],
    /// The only tools the agent may call.
    pub allowed: &'static [&'static str],
    /// Whether MCP and plugin tools reach the agent.
    pub extension_tools: ExtensionTools,
}

impl Permissions {
    /// The ruleset, ordered for the `findLast` evaluator.
    ///
    /// Order is not cosmetic. [`zuno_permission::evaluate`] and
    /// [`zuno_permission::visibility::is_tool_hidden`] both take the **last**
    /// matching rule, so the catch-all deny has to come first and the allows last;
    /// emitting them the other way round would produce a set that denies
    /// everything while reading like one that allows seven tools.
    #[must_use]
    pub fn rules(&self) -> Vec<Rule> {
        let mut rules = vec![rule("*", PermissionAction::Deny)];
        rules.extend(
            self.denied
                .iter()
                .map(|tool| rule(tool, PermissionAction::Deny)),
        );
        rules.extend(
            self.allowed
                .iter()
                .map(|tool| rule(tool, PermissionAction::Allow)),
        );
        rules
    }
}

fn rule(permission: &str, action: PermissionAction) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: "*".to_owned(),
        action,
    }
}

/// One entry in the lean roster.
///
/// Every field is required. That is the whole design: the table test in [`tests`]
/// iterates [`roster`] and checks each column, so an agent added without a
/// boundary, a temperature, a deny-by-default set, or an output contract fails to
/// compile or fails the suite — it cannot merge half-specified.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agent {
    /// The name a `task` call or `@` mention uses.
    pub name: &'static str,
    /// Position in the delegation graph.
    pub role: Role,
    /// Where the agent may be selected, in the upstream vocabulary.
    pub mode: AgentMode,
    /// Hidden from the `@` menu and from `agent list`.
    pub hidden: bool,
    /// Why a caller would choose this agent.
    pub description: &'static str,
    /// Why a caller would not.
    pub boundary: Boundary,
    /// Sampling temperature. Always declared; never inherited silently.
    pub temperature: f64,
    /// What the reply looks like.
    pub output: OutputContract,
    /// Tool policy.
    pub permissions: Permissions,
    /// Whether the agent may spawn children.
    pub delegation: Delegation,
    /// Whether the agent may write.
    pub write: Write,
    /// Whether the agent may gather its own context.
    pub research: Research,
    /// What must hold for the agent to be in the roster at all.
    pub gate: Gate,
}

impl Agent {
    /// The base ruleset, before extension tools are known.
    #[must_use]
    pub fn rules(&self) -> Vec<Rule> {
        self.permissions.rules()
    }

    /// The ruleset with the registry's extension tool ids folded in.
    ///
    /// A no-op for [`ExtensionTools::Excluded`] agents. See that variant's docs for
    /// why the seam exists at all.
    #[must_use]
    pub fn rules_with_extension_tools(&self, extension_tool_ids: &[&str]) -> Vec<Rule> {
        let mut rules = self.rules();
        if self.permissions.extension_tools == ExtensionTools::Inherit {
            rules.extend(
                extension_tool_ids
                    .iter()
                    .map(|tool| rule(tool, PermissionAction::Allow)),
            );
        }
        rules
    }

    /// One line for `agent list`, which a later todo renders as a command.
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "{name:<13} {mode:<9} temp={temperature:<4} {description}",
            name = self.name,
            mode = zuno_catalog::agent::mode_label(self.mode),
            temperature = self.temperature,
            description = self.description,
        )
    }

    /// Every string this agent contributes to a prompt or to `agent list`.
    ///
    /// Exists for the model-id scan in [`tests`]: a model name leaks into prose far
    /// more easily than into a struct field, so the check has to see the prose.
    #[must_use]
    pub fn rendered_strings(&self) -> Vec<String> {
        let mut strings = vec![
            self.name.to_owned(),
            self.description.to_owned(),
            self.boundary.render(),
        ];
        match self.output {
            OutputContract::Natural => {}
            OutputContract::EnginePrompt { prompt } => strings.push(prompt.to_owned()),
        }
        strings.push(self.summary_line());
        strings
    }

    /// The model-visible policy derived from the roster.
    ///
    /// The catalog owns the base role prompt. This block adds the negative
    /// delegation boundary from the same data the `task` validator reads.
    #[must_use]
    pub fn prompt_policy(&self) -> String {
        self.boundary.render()
    }
}

/// The names of the seven user-facing agents, in roster order.
pub const LEAN_NAMES: [&str; 7] = [
    "build",
    "deep",
    "explorer",
    "librarian",
    "advisor",
    "worker",
    "looker",
];

/// The engine's own agents, which the delegation roster carries unchanged.
///
/// The visible catalog entries are Zuno-native and align with this roster. `plan`
/// remains a primary mode rather than a delegation target; see [`internals`].
pub const INTERNAL_NAMES: [&str; 3] = ["compaction", "title", "summary"];

/// Tools denied by name to a reader confined to this repository.
///
/// `webfetch`/`websearch` are in here rather than merely uncovered because the
/// external-research lane belongs to [`LIBRARIAN`] alone: an explorer that can also
/// browse is an explorer that sometimes answers from a blog post instead of from
/// the code in front of it. `skill` is denied for the same reason the draft drops
/// per-call `load_skills` — skill access is a per-agent permission, so a lane that
/// needs no skills says so.
const READ_ONLY_DENIED: &[&str] = &[
    "bash",
    "edit",
    "task",
    "question",
    "plan_update",
    "todo_update",
    "execute",
    "plan_exit",
    "webfetch",
    "web_search",
    "skill",
];

/// The inspection tools every read-only agent may call.
const READ_ONLY_ALLOWED: &[&str] = &["read", "glob", "grep", "lsp", "plan_get", "todo_get"];

/// The primary, write-capable agent - the only one that may delegate.
pub const BUILD: Agent = Agent {
    name: "build",
    role: Role::Orchestrator,
    mode: AgentMode::Primary,
    hidden: false,
    description: "Owns the user's request end to end: decides what must be true, routes \
                  discovery and bounded execution to specialists, and synthesises the result. \
                  The only agent that may spawn children.",
    boundary: Boundary::DontDelegateWhen(
        "the whole task is smaller than the briefing it would take • you already have the \
         file path and need its contents • the answer is in this conversation • explaining \
         the task costs more than doing it • the work is one edit you are already mid-way \
         through.",
    ),
    temperature: 0.1,
    output: OutputContract::Natural,
    permissions: Permissions {
        denied: &["plan_exit"],
        allowed: &[
            "read",
            "glob",
            "grep",
            "lsp",
            "edit",
            "bash",
            "task",
            "webfetch",
            "web_search",
            "plan_get",
            "plan_update",
            "todo_get",
            "todo_update",
            "skill",
            "execute",
            "question",
        ],
        extension_tools: ExtensionTools::Inherit,
    },
    delegation: Delegation::MayDelegate,
    write: Write::Capable,
    research: Research::Allowed,
    gate: Gate::Always,
};

/// Thorough cross-cutting implementation without recursive delegation.
pub const DEEP: Agent = Agent {
    name: "deep",
    role: Role::Subagent,
    mode: AgentMode::Subagent,
    hidden: false,
    description: "Owns one difficult cross-cutting objective from evidence gathering through \
                  implementation and verification. Uses a larger investigation budget than a \
                  bounded worker but cannot spawn children.",
    boundary: Boundary::DontDelegateWhen(
        "the change is already well specified and local enough for `worker` • the caller needs \
         only repository locations or external research • the task still needs product \
         decisions from the primary session • the work can be verified in one small edit.",
    ),
    temperature: 0.1,
    output: OutputContract::Natural,
    permissions: Permissions {
        denied: &["task", "question", "plan_exit"],
        allowed: &[
            "read",
            "glob",
            "grep",
            "lsp",
            "edit",
            "bash",
            "webfetch",
            "web_search",
            "plan_get",
            "plan_update",
            "todo_get",
            "todo_update",
            "skill",
            "execute",
        ],
        extension_tools: ExtensionTools::Excluded,
    },
    delegation: Delegation::NoChildren,
    write: Write::Capable,
    research: Research::Allowed,
    gate: Gate::Always,
};

/// Read-only internal search.
pub const EXPLORER: Agent = Agent {
    name: "explorer",
    role: Role::Subagent,
    mode: AgentMode::Subagent,
    hidden: false,
    description: "Fast recon inside this repository. Answers \"where is X\", \"what exists \
                  under Y\", \"which call sites touch Z\", and returns a compressed map \
                  instead of file contents.",
    boundary: Boundary::DontDelegateWhen(
        "you know the path and want the bytes • you will need the full file anyway to edit \
         it • it is one lookup you can do in a single grep • the question is about an \
         external library rather than this tree.",
    ),
    temperature: 0.1,
    output: OutputContract::Natural,
    permissions: Permissions {
        denied: READ_ONLY_DENIED,
        allowed: READ_ONLY_ALLOWED,
        extension_tools: ExtensionTools::Excluded,
    },
    delegation: Delegation::NoChildren,
    write: Write::ReadOnly,
    research: Research::Confined,
    gate: Gate::Always,
};

/// Read-only external research.
pub const LIBRARIAN: Agent = Agent {
    name: "librarian",
    role: Role::Subagent,
    mode: AgentMode::Subagent,
    hidden: false,
    description: "Authority on anything outside this repository: current library \
                  documentation, API surfaces, release notes, and how other people solved \
                  the failure you are looking at.",
    boundary: Boundary::DontDelegateWhen(
        "it is a language feature or a stable standard-library API • you are confident and \
         the cost of being wrong is a compile error • the answer is already in this \
         conversation • the question is about this repository's own code.",
    ),
    temperature: 0.1,
    output: OutputContract::Natural,
    permissions: Permissions {
        denied: &[
            "bash",
            "edit",
            "task",
            "question",
            "plan_update",
            "todo_update",
            "execute",
            "plan_exit",
            "skill",
        ],
        allowed: &[
            "read",
            "glob",
            "grep",
            "lsp",
            "webfetch",
            "web_search",
            "plan_get",
            "todo_get",
        ],
        extension_tools: ExtensionTools::Excluded,
    },
    delegation: Delegation::NoChildren,
    write: Write::ReadOnly,
    research: Research::Allowed,
    gate: Gate::Always,
};

/// Read-only consulting and hostile review, in one lane.
pub const ADVISOR: Agent = Agent {
    name: "advisor",
    role: Role::Subagent,
    mode: AgentMode::Subagent,
    hidden: false,
    description: "Second opinion on high-stakes calls, and hostile review of work already \
                  done. Architecture trade-offs, a bug that survived two fixes, and \
                  \"tell me why this patch is wrong\" are the same lane: read the code, \
                  argue with it, name what it costs.",
    boundary: Boundary::DontDelegateWhen(
        "it is your first attempt at the bug • the trade-off has an obvious answer • you \
         want confirmation rather than disagreement • the decision is cheap to reverse • a \
         test would settle it faster than an argument.",
    ),
    // The one agent above the 0.1-0.2 band. Cutting `council` removed the only
    // place in slim's roster where two independent readings of a problem were
    // generated on purpose (`src/agents/orchestrator.ts:95` prices that at 3x cost,
    // which is why it is cut). The advisor inherits that job through its required
    // `<alternatives>` section, and at 0.1 a model collapses onto the single most
    // probable reading — exactly the failure the council existed to prevent. 0.4 is
    // enough divergence to surface a second option and well short of the 0.7 slim
    // spent on prose taste, where review starts inventing facts.
    temperature: 0.4,
    output: OutputContract::Natural,
    permissions: Permissions {
        denied: READ_ONLY_DENIED,
        allowed: READ_ONLY_ALLOWED,
        extension_tools: ExtensionTools::Excluded,
    },
    delegation: Delegation::NoChildren,
    write: Write::ReadOnly,
    research: Research::Confined,
    gate: Gate::Always,
};

/// Write-capable bounded execution — allowed to research, forbidden to delegate.
pub const WORKER: Agent = Agent {
    name: "worker",
    role: Role::Subagent,
    mode: AgentMode::Subagent,
    hidden: false,
    description: "Takes a bounded, well-specified change and finishes it: reads what it \
                  needs, edits, runs the checks, and iterates until they pass. Reports what \
                  it could not verify rather than claiming it did.",
    boundary: Boundary::DontDelegateWhen(
        "the requirements are still moving • it is one small edit in a file you have open • \
         the work is tangled with the change you are already making • what you actually need \
         is a decision, not an implementation • the task would need children of its own to \
         finish.",
    ),
    temperature: 0.1,
    output: OutputContract::Natural,
    permissions: Permissions {
        // `task` is the one capability a worker must not have: without it the lane
        // is bounded by construction, and with it the depth limit becomes the only
        // thing standing between one delegation and a fan-out tree.
        denied: &["task", "question", "plan_exit"],
        allowed: &[
            "read",
            "glob",
            "grep",
            "lsp",
            "edit",
            "bash",
            "webfetch",
            "web_search",
            "plan_get",
            "plan_update",
            "todo_get",
            "todo_update",
            "skill",
            "execute",
        ],
        extension_tools: ExtensionTools::Excluded,
    },
    delegation: Delegation::NoChildren,
    write: Write::Capable,
    research: Research::Allowed,
    gate: Gate::Always,
};

/// Multimodal reading, present whenever a vision-capable model is resolvable.
pub const LOOKER: Agent = Agent {
    name: "looker",
    role: Role::Subagent,
    mode: AgentMode::Subagent,
    hidden: false,
    description: "Reads images, screenshots, PDFs, and diagrams and returns text. Keeps \
                  megabytes of pixels out of the caller's context window and hands back only \
                  what was asked for.",
    boundary: Boundary::DontDelegateWhen(
        "the file is text a plain read handles • you need the exact bytes because you are \
         about to edit the file • the artifact is already described in this conversation • \
         one screenshot is the entire task and the round trip costs more than looking.",
    ),
    // Slightly above the floor: naming what is in an image needs some lexical
    // freedom, and at 0.1 descriptions collapse into clipped, templated phrases
    // that lose the detail the caller delegated for.
    temperature: 0.2,
    output: OutputContract::Natural,
    permissions: Permissions {
        denied: READ_ONLY_DENIED,
        allowed: READ_ONLY_ALLOWED,
        extension_tools: ExtensionTools::Excluded,
    },
    delegation: Delegation::NoChildren,
    write: Write::ReadOnly,
    research: Research::Confined,
    gate: Gate::VisionModel,
};

/// The seven named agents, gate ignored.
///
/// Use [`roster`] for the set that actually ships; this is the complete design, for
/// tests and for `agent list --all`.
#[must_use]
pub fn lean() -> Vec<Agent> {
    vec![BUILD, DEEP, EXPLORER, LIBRARIAN, ADVISOR, WORKER, LOOKER]
}

/// The engine's internal agents.
///
/// # Why exactly three, and why not `plan`
///
/// Upstream declares seven natives at `packages/opencode/src/agent/agent.ts:140-265`.
/// Three of them — `compaction`, `title`, `summary` — are `hidden: true`, take a
/// prompt, deny every tool, and are invoked by the engine rather than chosen by
/// anyone; dropping any of them silently removes auto-compaction, session titles,
/// or session summaries, with nothing else in the roster providing them. They are
/// carried here by reference to [`zuno_catalog::agent::builtin`] so the upstream
/// prompt text stays in exactly one place.
///
/// `plan` is a visible primary mode, not a task target. Its read-only edit policy
/// depends on a session-specific plan path, so the catalog and CLI composition root
/// own it instead of duplicating it in this static delegation roster.
#[must_use]
pub fn internals() -> Vec<Agent> {
    INTERNAL_NAMES
        .iter()
        .filter_map(|name| internal(name))
        .collect()
}

/// One internal agent, derived from the catalog's port of the upstream native.
fn internal(name: &str) -> Option<Agent> {
    let native = zuno_catalog::agent::builtin::get(name)?;
    let prompt = native.prompt?;
    Some(Agent {
        name: native.name,
        role: Role::Internal,
        mode: native.mode,
        hidden: native.hidden,
        description: match native.name {
            "compaction" => {
                "Engine-internal: rewrites a transcript that outgrew the context window."
            }
            "title" => {
                "Engine-internal: names a session from its first exchange, so a \
                         session list is readable."
            }
            _ => {
                "Engine-internal: summarises what a session accomplished, for the caller \
                  that resumes it."
            }
        },
        boundary: Boundary::NotDelegable {
            reason: "the engine invokes it at a fixed point in the turn loop; no caller \
                     chooses it, so there is no delegation decision to bound.",
        },
        // Upstream declares a temperature only for `title` (0.5, `agent.ts:239`)
        // and leaves the other two unset, i.e. at the provider default. This roster
        // requires a declared value from every agent — an undeclared temperature is
        // a per-provider behaviour difference nobody chose — so the two summarisers
        // take the floor, which is what deterministic condensation wants anyway.
        temperature: native.temperature.unwrap_or(0.1),
        output: OutputContract::EnginePrompt { prompt },
        permissions: Permissions {
            // `agent.ts:221`, `:241`, `:256`: all three deny everything. The
            // catch-all in `Permissions::rules` already does that; the named denies
            // make it legible, and there is nothing to allow.
            denied: GOVERNED_TOOL_IDS_SLICE,
            allowed: &[],
            extension_tools: ExtensionTools::Excluded,
        },
        delegation: Delegation::NoChildren,
        write: Write::ReadOnly,
        research: Research::Confined,
        gate: Gate::Always,
    })
}

/// [`GOVERNED_TOOL_IDS`] as a slice, for the internals' deny-everything set.
const GOVERNED_TOOL_IDS_SLICE: &[&str] = &GOVERNED_TOOL_IDS;

/// Whether a resolved model can be handed an image.
///
/// The signal is the catalog's own input-modality flag
/// ([`zuno_llm::catalog::resolved::ModalityFlags`], populated from models.dev's
/// `modalities.input` at `zuno-llm/src/catalog/merge.rs:194`). The adjacent
/// `attachment` flag is *not* the signal and using it would over-report: the pinned
/// catalog fixture contains models with `attachment: true` whose only input
/// modality is text (`zuno-llm/tests/fixtures/models-dev-pinned.json:145,160`), and
/// handing one an image produces a provider error, not a description.
#[must_use]
pub const fn is_vision_capable(capabilities: &ModelCapabilities) -> bool {
    capabilities.input.image
}

/// Whether any of the resolved models can be handed an image.
#[must_use]
pub fn any_vision_capable<'a, I>(capabilities: I) -> bool
where
    I: IntoIterator<Item = &'a ModelCapabilities>,
{
    capabilities.into_iter().any(is_vision_capable)
}

/// The roster that ships, given what the catalog resolved.
///
/// [`Gate::VisionModel`] entries appear exactly when `vision_available`. Callers
/// compute that with [`any_vision_capable`] over the resolved catalog rather than
/// reading a config key, because it is a capability question — see [`Gate`].
#[must_use]
pub fn roster(vision_available: bool) -> Vec<Agent> {
    lean()
        .into_iter()
        .filter(|agent| match agent.gate {
            Gate::Always => true,
            Gate::VisionModel => vision_available,
        })
        .chain(internals())
        .collect()
}

/// The agent named `name` in the roster for these capabilities.
#[must_use]
pub fn get(name: &str, vision_available: bool) -> Option<Agent> {
    roster(vision_available)
        .into_iter()
        .find(|agent| agent.name == name)
}

/// Valid `task` targets: everything a caller may actually name.
///
/// The `task` tool rejects a `subagent_type` outside this set, and rejects `build`
/// specifically: a coordinator cannot be a delegation target without reopening
/// the recursion this roster closes with
/// [`Delegation::NoChildren`].
#[must_use]
pub fn delegable(vision_available: bool) -> Vec<Agent> {
    roster(vision_available)
        .into_iter()
        .filter(|agent| agent.role == Role::Subagent)
        .collect()
}

/// The visible roster, rendered for `agent list`.
///
/// The command itself belongs to a later todo; this is the data and the rendering,
/// so that todo has nothing left to decide about the wording.
#[must_use]
pub fn render_list(vision_available: bool) -> String {
    let mut out = String::new();
    for agent in roster(vision_available) {
        if agent.hidden {
            continue;
        }
        out.push_str(&agent.summary_line());
        out.push('\n');
        out.push_str("              ");
        out.push_str(&agent.boundary.render());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests;

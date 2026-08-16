//! Which conditional tools reach the model, as predicates rather than branches.
//!
//! # Why this is a module and not four `if`s in the registry
//!
//! Four of the built-in tools are not always offered. Upstream expresses each
//! condition inline in the array literal that builds the tool list
//! (`packages/opencode/src/tool/registry.ts:226-244`), so the conditions are
//! unreachable from a test: to learn whether `plan_exit` is offered you have to
//! build the whole registry, which needs a database, a plugin host, and a live
//! config. The consequence upstream lives with is that no test states the
//! conditions; they are re-derived by reading the array.
//!
//! Here each condition is a named function of one plain data struct, so it is
//! callable, tested at both polarities, and consumable by the registry (todo 44)
//! without the registry restating it. [`exposure_predicate`] maps a wire id to its
//! predicate so the registry can filter a list without a `match` of its own.
//!
//! # The measured conditions
//!
//! Read off the real 1.18.12 binary rather than only off the source. `opencode debug
//! agent <name> --pure` prints the resolved `tools` map; the full 18-case transcript
//! is in `.omo/evidence/task-43-opencode-rust.txt`. Summary:
//!
//! | wire id | condition | oracle |
//! |---|---|---|
//! | `invalid` | always | `registry.ts:227` |
//! | `todowrite` | always | `registry.ts:237` |
//! | `question` | client ∈ {`app`,`cli`,`desktop`} **or** [`ENV_ENABLE_QUESTION_TOOL`] | `registry.ts:202,228` |
//! | `plan_exit` | plan mode **and** client == `cli` | `registry.ts:243` |
//!
//! `invalid` and `todowrite` really are unconditional, and their predicates say so
//! by returning `true` for every input. That is not a placeholder: the tests drive
//! the whole flag matrix through them, so gating one later fails a test instead of
//! passing silently.
//!
//! # Two layers, and only the first one is here
//!
//! `plan_exit` is offered by the registry whenever the condition above holds, and is
//! then hidden again from every agent except `plan` by the permission ruleset —
//! `plan_exit: "deny"` in the defaults, `plan_exit: "allow"` for the `plan` agent
//! (`packages/opencode/src/agent/agent.ts:128,164`). Measured: with
//! `OPENCODE_EXPERIMENTAL_PLAN_MODE=true` the tool is **absent** from `build` and
//! **present** on `plan`. That second layer is [`zuno_permission`]'s, so a caller of
//! this module that stops here will over-offer `plan_exit` on non-plan agents.
//! Todo 44 has to apply both, in that order.
//!
//! # Case and emptiness are load-bearing
//!
//! The client is compared as an exact string. `OPENCODE_CLIENT=CLI` offers neither
//! `question` nor `plan_exit`, and `OPENCODE_CLIENT=` (set but empty) offers neither
//! either — the `"cli"` default applies only when the variable is *absent*. Both
//! verified against the binary (cases 17 and 18), and both are the kind of thing a
//! well-meaning `eq_ignore_ascii_case` or `filter(|v| !v.is_empty())` would silently
//! change.

/// Selects the surface that started the process, which two tools are gated on.
///
/// Oracle: `Config.string("OPENCODE_CLIENT").pipe(Config.withDefault("cli"))`
/// (`packages/opencode/src/effect/runtime-flags.ts:57`).
pub const ENV_CLIENT: &str = "OPENCODE_CLIENT";

/// Offers `question` regardless of the client.
///
/// Oracle: `flags.enableQuestionTool` (`runtime-flags.ts:41`), the second half of
/// `registry.ts:202`'s disjunction.
pub const ENV_ENABLE_QUESTION_TOOL: &str = "OPENCODE_ENABLE_QUESTION_TOOL";

/// The blanket experimental switch, consulted only when the specific flag is unset.
///
/// See [`ExposureFlags::from_lookup`] for the precedence, which is not the obvious
/// one.
pub const ENV_EXPERIMENTAL: &str = "OPENCODE_EXPERIMENTAL";

/// Enables plan mode, the first half of `plan_exit`'s condition.
///
/// Oracle: `experimentalPlanMode: enabledByExperimental("OPENCODE_EXPERIMENTAL_PLAN_MODE")`
/// (`runtime-flags.ts:48`).
pub const ENV_EXPERIMENTAL_PLAN_MODE: &str = "OPENCODE_EXPERIMENTAL_PLAN_MODE";

/// The client assumed when [`ENV_CLIENT`] is absent.
pub const DEFAULT_CLIENT: &str = "cli";

/// The clients that get `question` without an explicit flag.
///
/// Oracle: `["app", "cli", "desktop"].includes(flags.client)` (`registry.ts:202`).
pub const QUESTION_CLIENTS: [&str; 3] = ["app", "cli", "desktop"];

/// The only client that gets `plan_exit`.
///
/// Oracle: `flags.client === "cli"` (`registry.ts:243`). Narrower than
/// [`QUESTION_CLIENTS`] on purpose — `app` and `desktop` get `question` but not
/// `plan_exit`, verified as cases 10 and 4 of the transcript.
pub const PLAN_EXIT_CLIENT: &str = "cli";

/// The surface that started the process.
///
/// A newtype over the raw string rather than a closed enum, because upstream's flag
/// is a bare `Config.string` with no validation: an unrecognised client is a normal
/// state that simply matches no gate, not an error. Keeping the raw value means a
/// caller can log or forward exactly what it was given.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Client(String);

impl Client {
    /// A client from its wire name, kept verbatim.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The client assumed when [`ENV_CLIENT`] is absent.
    #[must_use]
    pub fn default_cli() -> Self {
        Self::new(DEFAULT_CLIENT)
    }

    /// The wire name, exactly as it was supplied.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the CLI, the only client `plan_exit` is offered to.
    #[must_use]
    pub fn is_plan_exit_client(&self) -> bool {
        self.0 == PLAN_EXIT_CLIENT
    }

    /// Whether this client can render an interactive question.
    ///
    /// The three surfaces that have somewhere to draw a prompt. A headless client —
    /// `tui` in the measured transcript, or anything unrecognised — cannot, so the
    /// tool is withheld rather than offered and then failing at call time.
    #[must_use]
    pub fn can_render_questions(&self) -> bool {
        QUESTION_CLIENTS.contains(&self.0.as_str())
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::default_cli()
    }
}

impl std::fmt::Display for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything the exposure predicates read, resolved once.
///
/// Held as data rather than read from `std::env` inside each predicate, for the same
/// reason [`crate::websearch::gating::SearchConfig`] is: Rust 2024 makes
/// `env::set_var` `unsafe` and this workspace forbids `unsafe_code`, so a test that
/// had to mutate the environment could not be written at all — let alone run
/// concurrently in a shared test binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureFlags {
    /// The surface that started the process.
    pub client: Client,
    /// Offer `question` whatever the client is.
    pub enable_question_tool: bool,
    /// Plan mode, the first half of `plan_exit`'s condition.
    pub experimental_plan_mode: bool,
}

impl Default for ExposureFlags {
    /// What a bare `opencode` invocation resolves to: CLI client, no flags.
    ///
    /// Not `#[derive(Default)]`, because the client's default is `cli` rather than
    /// the empty string, and an empty client matches no gate at all.
    fn default() -> Self {
        Self {
            client: Client::default_cli(),
            enable_question_tool: false,
            experimental_plan_mode: false,
        }
    }
}

impl ExposureFlags {
    /// Reads the flags from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        let env = zuno_paths::Env::from_process();
        Self::from_lookup(|key| env.value(key).map(str::to_owned))
    }

    /// Reads the flags through a caller-supplied lookup, for tests and for a host
    /// that sources them from somewhere other than the environment.
    ///
    /// # The precedence that is not obvious
    ///
    /// `OPENCODE_EXPERIMENTAL` is a *fallback*, not an override. Upstream's
    /// `enabledByExperimental` (`runtime-flags.ts:11-15`) reads the specific flag as
    /// an `Option` and only substitutes the blanket switch when it is absent, so
    /// `OPENCODE_EXPERIMENTAL=true OPENCODE_EXPERIMENTAL_PLAN_MODE=false` leaves plan
    /// mode **off**. Verified against the binary as transcript case 13 — a reading
    /// that treated the blanket switch as a disjunct would have offered `plan_exit`
    /// there.
    ///
    /// A variable that is present but unparseable as a boolean counts as absent, so
    /// `OPENCODE_EXPERIMENTAL_PLAN_MODE=maybe` falls through to the blanket switch.
    #[must_use]
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let experimental = lookup(ENV_EXPERIMENTAL)
            .as_deref()
            .and_then(parse_bool)
            .unwrap_or(false);
        let enabled_by_experimental = |key: &str| {
            lookup(key)
                .as_deref()
                .and_then(parse_bool)
                .unwrap_or(experimental)
        };

        Self {
            client: lookup(ENV_CLIENT).map_or_else(Client::default_cli, Client::new),
            enable_question_tool: lookup(ENV_ENABLE_QUESTION_TOOL)
                .as_deref()
                .and_then(parse_bool)
                .unwrap_or(false),
            experimental_plan_mode: enabled_by_experimental(ENV_EXPERIMENTAL_PLAN_MODE),
        }
    }

    /// The flags with a different client, for building a matrix in a test.
    #[must_use]
    pub fn with_client(mut self, client: impl Into<String>) -> Self {
        self.client = Client::new(client);
        self
    }

    /// The flags with plan mode forced on.
    #[must_use]
    pub fn with_plan_mode(mut self) -> Self {
        self.experimental_plan_mode = true;
        self
    }

    /// The flags with the question-tool override forced on.
    #[must_use]
    pub fn with_question_tool(mut self) -> Self {
        self.enable_question_tool = true;
        self
    }
}

/// How an env value is read as a boolean.
///
/// Returns `None` for absent-equivalent input so a caller can distinguish "unset"
/// from "explicitly false" — the distinction
/// [`ExposureFlags::from_lookup`]'s fallback rule turns on. Effect's
/// `Config.boolean` accepts these spellings and rejects everything else; an empty
/// value is treated as unset, which is what `Config.option` yields for it.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Whether `invalid` is offered. Always.
///
/// Oracle: `registry.ts:227` lists `tool.invalid` first and unguarded. It *is*
/// model-visible — verified present in every one of the 18 measured cases — and its
/// description is the single line "Do not use". The tool exists so a malformed call
/// has somewhere to land with a message the model can act on, which requires the
/// model to have been told the name.
///
/// Takes the flags it ignores so the registry can treat all four predicates
/// uniformly, and so a future gate has one obvious place to go.
#[must_use]
pub fn exposes_invalid(_flags: &ExposureFlags) -> bool {
    true
}

/// Whether `todowrite` is offered. Always.
///
/// Oracle: `registry.ts:237` lists `tool.todo` unguarded. Verified present in all 18
/// measured cases, including with the client set to `tui` and with every
/// experimental flag off.
#[must_use]
pub fn exposes_todowrite(_flags: &ExposureFlags) -> bool {
    true
}

/// Whether `question` is offered.
///
/// Oracle, verbatim in structure (`registry.ts:202`):
///
/// ```text
/// ["app", "cli", "desktop"].includes(flags.client) || flags.enableQuestionTool
/// ```
///
/// A disjunction: the flag rescues a client that cannot otherwise render a prompt,
/// which is how the tool is testable from a `tui` host.
#[must_use]
pub fn exposes_question(flags: &ExposureFlags) -> bool {
    flags.client.can_render_questions() || flags.enable_question_tool
}

/// Whether `plan_exit` is offered.
///
/// Oracle, verbatim in structure (`registry.ts:243`):
///
/// ```text
/// flags.experimentalPlanMode && flags.client === "cli"
/// ```
///
/// A **conjunction**, and unlike [`exposes_question`] there is no flag that rescues a
/// non-CLI client: `OPENCODE_CLIENT=app` with plan mode on offers `question` and
/// withholds `plan_exit` (transcript cases 10 and 4). Remember the second layer
/// documented on this module — the permission ruleset withholds it again from every
/// agent but `plan`.
#[must_use]
pub fn exposes_plan_exit(flags: &ExposureFlags) -> bool {
    flags.experimental_plan_mode && flags.client.is_plan_exit_client()
}

/// The shape every exposure condition has.
///
/// A plain `fn` pointer rather than a boxed closure, so [`CONDITIONAL_TOOLS`] can be a
/// `const` and a caller can compare two predicates for identity.
pub type ExposurePredicate = fn(&ExposureFlags) -> bool;

/// Every conditional tool this module gates, as `(wire id, predicate)`.
///
/// The registry's element type is keyed by [`zuno_tool::Tool::id`], which is the wire
/// id, so these are wire ids and not upstream's internal registry keys. Two of the
/// four differ: upstream keys `todowrite` as `todo` (`registry.ts:214`) and
/// `plan_exit` as `plan` (`registry.ts:220`), while `invalid` and `question` are the
/// same in both spaces.
pub const CONDITIONAL_TOOLS: [(&str, ExposurePredicate); 4] = [
    (crate::invalid::WIRE_ID, exposes_invalid),
    (crate::question::WIRE_ID, exposes_question),
    (crate::todo::WIRE_ID, exposes_todowrite),
    (crate::plan_exit::WIRE_ID, exposes_plan_exit),
];

/// The predicate gating `wire_id`, or `None` when the tool is not one of these four.
///
/// Todo 44's filter is meant to be `predicate(&flags)` for the tools this returns
/// something for, and unconditional for the rest — one lookup instead of a `match`
/// the registry would have to keep in step with this module.
#[must_use]
pub fn exposure_predicate(wire_id: &str) -> Option<fn(&ExposureFlags) -> bool> {
    CONDITIONAL_TOOLS
        .iter()
        .find(|(id, _)| *id == wire_id)
        .map(|(_, predicate)| *predicate)
}

/// The wire ids offered under `flags`, in [`CONDITIONAL_TOOLS`] order.
///
/// The shape a differential compares: one call yields the set the registry would
/// contain, so a test states a flag configuration and asserts a list rather than
/// four separate booleans.
#[must_use]
pub fn exposed_conditional_tools(flags: &ExposureFlags) -> Vec<&'static str> {
    CONDITIONAL_TOOLS
        .iter()
        .filter(|(_, predicate)| predicate(flags))
        .map(|(id, _)| *id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(pairs: &[(&str, &str)]) -> ExposureFlags {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        ExposureFlags::from_lookup(|key| {
            owned
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        })
    }

    /// The flag configurations a predicate that ignores its input must survive.
    fn matrix() -> Vec<ExposureFlags> {
        let mut all = Vec::new();
        for client in ["cli", "app", "desktop", "tui", "CLI", "", "unknown"] {
            for plan in [false, true] {
                for question in [false, true] {
                    all.push(ExposureFlags {
                        client: Client::new(client),
                        enable_question_tool: question,
                        experimental_plan_mode: plan,
                    });
                }
            }
        }
        all
    }

    // --- invalid: unconditional, and the test says so across the whole matrix ---

    #[test]
    fn conditional_invalid_is_present_under_its_enabling_condition() {
        // Its condition is "always", so the default configuration is the enabling one.
        assert!(exposes_invalid(&ExposureFlags::default()));
        assert!(exposed_conditional_tools(&ExposureFlags::default()).contains(&"invalid"));
    }

    #[test]
    fn conditional_invalid_is_never_absent_for_any_flag_configuration() {
        // The absence case for an unconditional tool: there is none. Asserted over
        // the full matrix so that gating it later fails here instead of silently
        // shrinking the model's tool list.
        for configuration in matrix() {
            assert!(
                exposes_invalid(&configuration),
                "invalid must be offered for {configuration:?}"
            );
        }
    }

    // --- todowrite: same shape ---

    #[test]
    fn conditional_todowrite_is_present_under_its_enabling_condition() {
        assert!(exposes_todowrite(&ExposureFlags::default()));
        assert!(exposed_conditional_tools(&ExposureFlags::default()).contains(&"todowrite"));
    }

    #[test]
    fn conditional_todowrite_is_never_absent_for_any_flag_configuration() {
        for configuration in matrix() {
            assert!(
                exposes_todowrite(&configuration),
                "todowrite must be offered for {configuration:?}"
            );
        }
    }

    // --- question: client disjunction ---

    #[test]
    fn conditional_question_is_present_for_each_interactive_client() {
        for client in QUESTION_CLIENTS {
            let configuration = ExposureFlags::default().with_client(client);
            assert!(
                exposes_question(&configuration),
                "question must be offered to the {client} client"
            );
        }
    }

    #[test]
    fn conditional_question_is_present_when_the_override_flag_is_set() {
        let configuration = ExposureFlags::default()
            .with_client("tui")
            .with_question_tool();
        assert!(exposes_question(&configuration));
    }

    #[test]
    fn conditional_question_is_absent_for_a_client_that_cannot_render_it() {
        // Transcript case 2: OPENCODE_CLIENT=tui drops `question` from the list.
        let configuration = ExposureFlags::default().with_client("tui");
        assert!(!exposes_question(&configuration));
        assert!(!exposed_conditional_tools(&configuration).contains(&"question"));
    }

    #[test]
    fn conditional_question_is_absent_for_an_unrecognised_client() {
        assert!(!exposes_question(
            &ExposureFlags::default().with_client("headless")
        ));
    }

    #[test]
    fn conditional_question_client_matching_is_case_sensitive() {
        // Transcript case 17: OPENCODE_CLIENT=CLI offers neither conditional tool.
        let configuration = ExposureFlags::default().with_client("CLI").with_plan_mode();
        assert!(!exposes_question(&configuration));
        assert!(!exposes_plan_exit(&configuration));
    }

    #[test]
    fn conditional_question_is_absent_for_an_empty_client() {
        // Transcript case 18: set-but-empty is not defaulted to `cli`.
        assert!(!exposes_question(&ExposureFlags::default().with_client("")));
    }

    // --- plan_exit: conjunction, so two distinct absence cases ---

    #[test]
    fn conditional_plan_exit_is_present_under_plan_mode_with_a_cli_client() {
        // The task's happy path: OPENCODE_CLIENT=cli and plan mode enabled.
        let configuration = flags(&[(ENV_CLIENT, "cli"), (ENV_EXPERIMENTAL_PLAN_MODE, "true")]);
        assert!(exposes_plan_exit(&configuration));
        assert!(exposed_conditional_tools(&configuration).contains(&"plan_exit"));
    }

    #[test]
    fn conditional_plan_exit_is_absent_without_plan_mode() {
        // First half of the conjunction fails. Transcript case 11.
        let configuration = flags(&[(ENV_CLIENT, "cli")]);
        assert!(!configuration.experimental_plan_mode);
        assert!(!exposes_plan_exit(&configuration));
        assert!(!exposed_conditional_tools(&configuration).contains(&"plan_exit"));
    }

    #[test]
    fn conditional_plan_exit_is_absent_for_a_non_cli_client_even_in_plan_mode() {
        // Second half of the conjunction fails. Transcript cases 9 and 10: `app` and
        // `desktop` get `question` and still not `plan_exit`.
        for client in ["tui", "app", "desktop"] {
            let configuration = ExposureFlags::default()
                .with_client(client)
                .with_plan_mode();
            assert!(
                !exposes_plan_exit(&configuration),
                "plan_exit must not be offered to the {client} client"
            );
        }
        assert!(exposes_question(
            &ExposureFlags::default().with_client("app").with_plan_mode()
        ));
    }

    #[test]
    fn conditional_plan_exit_needs_both_halves_not_either() {
        assert!(!exposes_plan_exit(&ExposureFlags::default()));
        assert!(!exposes_plan_exit(
            &ExposureFlags::default().with_client("tui").with_plan_mode()
        ));
        assert!(exposes_plan_exit(
            &ExposureFlags::default().with_plan_mode()
        ));
    }

    // --- the flag reader ---

    #[test]
    fn an_absent_client_defaults_to_cli() {
        assert_eq!(flags(&[]).client.as_str(), DEFAULT_CLIENT);
        assert!(exposes_question(&flags(&[])));
    }

    #[test]
    fn a_present_but_empty_client_is_not_defaulted() {
        let configuration = flags(&[(ENV_CLIENT, "")]);
        assert_eq!(configuration.client.as_str(), "");
        assert!(!exposes_question(&configuration));
    }

    #[test]
    fn the_blanket_experimental_switch_enables_plan_mode_when_the_specific_flag_is_unset() {
        // Transcript case 12.
        assert!(flags(&[(ENV_EXPERIMENTAL, "true")]).experimental_plan_mode);
    }

    #[test]
    fn an_explicit_false_beats_the_blanket_experimental_switch() {
        // Transcript case 13 — the precedence a disjunction would get wrong.
        let configuration = flags(&[
            (ENV_EXPERIMENTAL, "true"),
            (ENV_EXPERIMENTAL_PLAN_MODE, "false"),
        ]);
        assert!(!configuration.experimental_plan_mode);
        assert!(!exposes_plan_exit(&configuration));
    }

    #[test]
    fn an_explicit_true_survives_a_false_blanket_switch() {
        // Transcript case 14.
        let configuration = flags(&[
            (ENV_EXPERIMENTAL, "false"),
            (ENV_EXPERIMENTAL_PLAN_MODE, "true"),
        ]);
        assert!(exposes_plan_exit(&configuration));
    }

    #[test]
    fn numeric_flag_spellings_are_honoured() {
        // Transcript cases 15 and 16.
        assert!(flags(&[(ENV_EXPERIMENTAL_PLAN_MODE, "1")]).experimental_plan_mode);
        assert!(!flags(&[(ENV_EXPERIMENTAL_PLAN_MODE, "0")]).experimental_plan_mode);
    }

    #[test]
    fn an_unparseable_flag_value_falls_through_to_the_blanket_switch() {
        assert!(
            flags(&[
                (ENV_EXPERIMENTAL, "true"),
                (ENV_EXPERIMENTAL_PLAN_MODE, "maybe"),
            ])
            .experimental_plan_mode
        );
        assert!(!flags(&[(ENV_EXPERIMENTAL_PLAN_MODE, "maybe")]).experimental_plan_mode);
    }

    #[test]
    fn parse_bool_distinguishes_unset_from_false() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("FALSE"), Some(false));
        assert_eq!(parse_bool(" on "), Some(true));
        assert_eq!(parse_bool(""), None);
        assert_eq!(parse_bool("nope"), None);
    }

    // --- the registry-facing surface ---

    #[test]
    fn conditional_every_gated_tool_has_a_predicate_reachable_by_wire_id() {
        for wire_id in ["invalid", "question", "todowrite", "plan_exit"] {
            assert!(
                exposure_predicate(wire_id).is_some(),
                "{wire_id} must be reachable by its wire id"
            );
        }
    }

    #[test]
    fn conditional_an_unconditional_tool_has_no_predicate() {
        // `read` and friends are not gated; the registry must not be handed a
        // predicate for them and silently start filtering.
        for wire_id in ["read", "write", "glob", "grep", "todo", "plan"] {
            assert!(
                exposure_predicate(wire_id).is_none(),
                "{wire_id} is not one of the four conditional tools"
            );
        }
    }

    #[test]
    fn conditional_the_wire_ids_are_the_wire_ids_and_not_the_registry_keys() {
        let ids: Vec<&str> = CONDITIONAL_TOOLS.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec!["invalid", "question", "todowrite", "plan_exit"]);
        // Upstream's registry keys for two of them; neither is a wire id.
        assert!(!ids.contains(&"todo"));
        assert!(!ids.contains(&"plan"));
    }

    #[test]
    fn conditional_the_default_configuration_matches_the_measured_baseline() {
        // Transcript case 1: a bare invocation offers invalid, question, todowrite
        // and withholds plan_exit.
        assert_eq!(
            exposed_conditional_tools(&ExposureFlags::default()),
            vec!["invalid", "question", "todowrite"]
        );
    }

    #[test]
    fn conditional_the_full_plan_mode_cli_configuration_offers_all_four() {
        assert_eq!(
            exposed_conditional_tools(&ExposureFlags::default().with_plan_mode()),
            vec!["invalid", "question", "todowrite", "plan_exit"]
        );
    }

    #[test]
    fn conditional_a_tui_host_in_plan_mode_offers_only_the_unconditional_two() {
        // Transcript case 9.
        assert_eq!(
            exposed_conditional_tools(
                &ExposureFlags::default().with_client("tui").with_plan_mode()
            ),
            vec!["invalid", "todowrite"]
        );
    }
}

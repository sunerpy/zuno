//! Unit tests for command resolution.
//!
//! The precedence tests are the point of todo 15, so each level of the chain has
//! a test that isolates it, and the skills-only-if-free rule has three: against
//! a built-in, against a config command, and against an MCP prompt.
//!
//! Every expected value here was observed on the real `opencode` binary
//! (`GET /command`, version 1.18.12, source tree pinned `aefaf140c1` = v1.18.13)
//! or produced by a verbatim transcription of its expansion code. Nothing is
//! inferred. Transcripts: `.omo/evidence/task-15-opencode-rust.txt`.

use super::*;

const WORKTREE: &str = "/tmp/worktree";

fn config(entries: &[(&str, CommandConfig)]) -> OrderedMap<CommandConfig> {
    let mut map = OrderedMap::new();
    for (name, entry) in entries {
        map.insert(*name, entry.clone());
    }
    map
}

fn entry(template: &str) -> CommandConfig {
    CommandConfig {
        template: template.to_owned(),
        description: None,
        agent: None,
        model: None,
        variant: None,
        subtask: None,
    }
}

fn described(template: &str, description: &str) -> CommandConfig {
    CommandConfig {
        description: Some(description.to_owned()),
        ..entry(template)
    }
}

fn skill(name: &str, content: &str) -> SkillCommand {
    SkillCommand {
        name: name.to_owned(),
        description: Some(format!("SKILL DESC {name}")),
        content: content.to_owned(),
        location: SkillLocation::File(PathBuf::from(format!("/skills/{name}/SKILL.md"))),
    }
}

fn prompt(client: &str, name: &str, arguments: &[&str]) -> McpPrompt {
    McpPrompt {
        client: client.to_owned(),
        prompt: name.to_owned(),
        description: Some(format!("MCP PROMPT {name}")),
        arguments: arguments.iter().map(|a| (*a).to_owned()).collect(),
    }
}

fn text_of(info: &Info) -> &str {
    match &info.template {
        Template::Text(text) => text,
        Template::Mcp(_) => panic!("expected a text template, got an MCP one"),
    }
}

// ---------------------------------------------------------------------------
// Level 1 — built-ins
// ---------------------------------------------------------------------------

#[test]
fn builtins_seed_the_registry() {
    let registry = Registry::build(&Sources::new(WORKTREE));

    assert_eq!(
        registry.names().collect::<Vec<_>>(),
        vec!["init", "review"],
        "only init and review are built in"
    );
}

#[test]
fn builtin_init_matches_the_observed_shape() {
    let registry = Registry::build(&Sources::new(WORKTREE));
    let init = registry.get("init").expect("init is built in");

    assert_eq!(init.description.as_deref(), Some("guided AGENTS.md setup"));
    assert_eq!(init.source, Source::Command);
    assert_eq!(init.subtask, None);
    assert_eq!(init.hints, vec!["$ARGUMENTS".to_owned()]);
    assert_eq!(
        init.agent, None,
        "the built-in pins no agent, so the session's is used"
    );
    assert_eq!(init.model, None);
}

#[test]
fn builtin_init_names_zuno_as_the_future_agent() {
    let registry = Registry::build(&Sources::new(WORKTREE));
    let template = text_of(registry.get("init").expect("init is built in"));

    assert!(template.contains("future Zuno sessions"));
    assert!(!template.contains("future OpenCode sessions"));
}

#[test]
fn builtin_review_runs_as_a_subtask() {
    let registry = Registry::build(&Sources::new(WORKTREE));
    let review = registry.get("review").expect("review is built in");

    assert_eq!(
        review.description.as_deref(),
        Some("review changes [commit|branch|pr], defaults to uncommitted")
    );
    assert_eq!(
        review.subtask,
        Some(true),
        "command/index.ts:86, and observed as subtask=True on the real binary"
    );
    assert_eq!(review.hints, vec!["$ARGUMENTS".to_owned()]);
}

#[test]
fn builtin_init_interpolates_the_worktree_exactly_once() {
    let registry = Registry::build(&Sources::new(WORKTREE));
    let template = text_of(registry.get("init").expect("init is built in"));

    assert!(
        template.contains(&format!("already exists at `{WORKTREE}`")),
        "the ${{path}} placeholder is filled with the worktree"
    );
    assert!(
        !template.contains("${path}"),
        "no placeholder survives: initialize.txt has exactly one"
    );
    // Observed template length on the real binary for worktree /tmp/oc15/clean
    // was 3500 chars; the file is 3492, and 3492 - 7 + 15 = 3500.
    assert_eq!(
        template.chars().count(),
        TEMPLATE_INITIALIZE.chars().count() - "${path}".len() + WORKTREE.len()
    );
}

#[test]
fn builtin_review_has_no_worktree_placeholder() {
    let registry = Registry::build(&Sources::new(WORKTREE));
    let template = text_of(registry.get("review").expect("review is built in"));

    assert_eq!(
        template, TEMPLATE_REVIEW,
        "review.txt has no ${{path}}, so its substitution is a no-op"
    );
    // Observed on the real binary: 4704 characters.
    assert_eq!(template.chars().count(), 4704);
}

// ---------------------------------------------------------------------------
// Level 2 — config commands
// ---------------------------------------------------------------------------

#[test]
fn config_command_overrides_a_builtin() {
    let cfg = config(&[(
        "review",
        described("CONFIG REVIEW $ARGUMENTS", "CONFIG DESC"),
    )]);
    let registry = Registry::build(&Sources::new(WORKTREE).with_config(Some(&cfg)));
    let review = registry.get("review").expect("review resolves");

    assert_eq!(text_of(review), "CONFIG REVIEW $ARGUMENTS");
    assert_eq!(review.description.as_deref(), Some("CONFIG DESC"));
    assert_eq!(
        review.subtask, None,
        "the config entry replaces the built-in wholesale, losing subtask=true"
    );
}

#[test]
fn overriding_a_builtin_keeps_its_listing_position() {
    let cfg = config(&[
        ("zzz", entry("LAST")),
        ("review", entry("OVERRIDE")),
        ("aaa", entry("MIDDLE")),
    ]);
    let registry = Registry::build(&Sources::new(WORKTREE).with_config(Some(&cfg)));

    assert_eq!(
        registry.names().collect::<Vec<_>>(),
        vec!["init", "review", "zzz", "aaa"],
        "review stays in slot 1 where the built-in put it; observed on the real binary"
    );
}

#[test]
fn config_command_carries_agent_model_and_subtask() {
    let cfg = config(&[(
        "custom",
        CommandConfig {
            template: "T".to_owned(),
            description: Some("D".to_owned()),
            agent: Some("plan".to_owned()),
            model: Some("anthropic/claude".to_owned()),
            variant: Some("max".to_owned()),
            subtask: Some(true),
        },
    )]);
    let registry = Registry::build(&Sources::new(WORKTREE).with_config(Some(&cfg)));
    let custom = registry.get("custom").expect("custom resolves");

    assert_eq!(custom.agent.as_deref(), Some("plan"));
    assert_eq!(custom.model.as_deref(), Some("anthropic/claude"));
    assert_eq!(custom.subtask, Some(true));
    // `variant` is intentionally not on Info: command/index.ts:91-102 does not
    // copy it, so exposing it here would invent a field the oracle lacks.
}

// ---------------------------------------------------------------------------
// Level 3 — MCP prompts
// ---------------------------------------------------------------------------

#[test]
fn mcp_prompt_is_keyed_by_server_and_name() {
    let prompts = [prompt("srv", "hello", &["alpha", "beta"])];
    let registry = Registry::build(&Sources::new(WORKTREE).with_mcp_prompts(&prompts));

    assert!(
        registry.get("srv:hello").is_some(),
        "mcp/catalog.ts:100-105 keys prompts client:name"
    );
    assert!(
        registry.get("hello").is_none(),
        "the bare prompt name is not a command"
    );
}

#[test]
fn mcp_prompt_overrides_a_config_command() {
    let cfg = config(&[(
        "srv:hello",
        described("CONFIG TEMPLATE that must lose", "CONFIG DESC"),
    )]);
    let prompts = [prompt("srv", "hello", &["alpha", "beta"])];
    let registry = Registry::build(
        &Sources::new(WORKTREE)
            .with_config(Some(&cfg))
            .with_mcp_prompts(&prompts),
    );
    let resolved = registry.get("srv:hello").expect("srv:hello resolves");

    assert_eq!(
        resolved.source,
        Source::Mcp,
        "level 3 overwrites level 2 unconditionally; observed on the real binary"
    );
    assert_eq!(resolved.description.as_deref(), Some("MCP PROMPT hello"));
    assert!(matches!(resolved.template, Template::Mcp(_)));
    assert_eq!(
        registry.len(),
        3,
        "the collision replaces rather than duplicating"
    );
}

#[test]
fn mcp_arguments_map_onto_positionals() {
    let prompts = [prompt("srv", "hello", &["alpha", "beta", "gamma"])];
    let registry = Registry::build(&Sources::new(WORKTREE).with_mcp_prompts(&prompts));
    let resolved = registry.get("srv:hello").expect("srv:hello resolves");

    let Template::Mcp(mcp) = &resolved.template else {
        panic!("an MCP prompt yields an MCP template");
    };
    assert_eq!(
        mcp.arguments,
        vec![
            ("alpha".to_owned(), "$1".to_owned()),
            ("beta".to_owned(), "$2".to_owned()),
            ("gamma".to_owned(), "$3".to_owned()),
        ],
        "command/index.ts:117 binds the i-th declared argument to $(i+1)"
    );
    assert_eq!(mcp.client, "srv");
    assert_eq!(mcp.prompt, "hello");
    assert_eq!(
        resolved.hints,
        vec!["$1".to_owned(), "$2".to_owned(), "$3".to_owned()],
        "hints come from the argument count, not the prompt text"
    );
}

#[test]
fn mcp_prompt_without_arguments_has_no_hints() {
    let prompts = [prompt("srv", "noargs", &[])];
    let registry = Registry::build(&Sources::new(WORKTREE).with_mcp_prompts(&prompts));
    let resolved = registry.get("srv:noargs").expect("srv:noargs resolves");

    assert!(resolved.hints.is_empty());
    let Template::Mcp(mcp) = &resolved.template else {
        panic!("an MCP prompt yields an MCP template");
    };
    assert!(
        mcp.arguments.is_empty(),
        "the oracle sends {{}} when arguments are absent"
    );
}

#[test]
fn mcp_names_are_sanitized() {
    let prompts = [prompt("my server.v2", "do it!", &[])];
    let registry = Registry::build(&Sources::new(WORKTREE).with_mcp_prompts(&prompts));

    assert!(
        registry.get("my_server_v2:do_it_").is_some(),
        "every character outside [A-Za-z0-9_-] becomes _, the colon excepted"
    );
}

#[test]
fn sanitize_replaces_only_disallowed_characters() {
    assert_eq!(sanitize("keep-Me_09"), "keep-Me_09");
    assert_eq!(sanitize("a.b c:d"), "a_b_c_d");
    assert_eq!(
        sanitize("日本"),
        "__",
        "non-ASCII is replaced per character"
    );
}

// ---------------------------------------------------------------------------
// Level 4 — skills, only when the name is free. The headline of todo 15.
// ---------------------------------------------------------------------------

#[test]
fn skill_never_overrides_a_config_command() {
    let cfg = config(&[(
        "collide",
        described("CONFIG TEMPLATE collide $1", "CONFIG DESC collide"),
    )]);
    let skills = [skill("collide", "SKILL BODY that must not win")];
    let registry = Registry::build(
        &Sources::new(WORKTREE)
            .with_config(Some(&cfg))
            .with_skills(&skills),
    );
    let resolved = registry.get("collide").expect("collide resolves");

    assert_eq!(
        resolved.source,
        Source::Command,
        "command/index.ts:135 skips a skill whose name is taken"
    );
    assert_eq!(text_of(resolved), "CONFIG TEMPLATE collide $1");
    assert_eq!(
        resolved.description.as_deref(),
        Some("CONFIG DESC collide"),
        "not even the description leaks from the skill"
    );
    assert_eq!(
        registry.len(),
        3,
        "the skill is dropped entirely, not listed twice"
    );
}

#[test]
fn skill_never_overrides_a_builtin() {
    let skills = [
        skill("init", "SKILL BODY init"),
        skill("review", "SKILL BODY review"),
    ];
    let registry = Registry::build(&Sources::new(WORKTREE).with_skills(&skills));

    assert_eq!(
        registry.get("init").expect("init resolves").source,
        Source::Command
    );
    assert_eq!(
        registry.get("review").expect("review resolves").source,
        Source::Command
    );
    assert_eq!(registry.len(), 2, "both skills are dropped");
}

#[test]
fn skill_never_overrides_an_mcp_prompt() {
    let prompts = [prompt("srv", "noargs", &[])];
    let skills = [skill("srv:noargs", "SKILL BODY that must not win")];
    let registry = Registry::build(
        &Sources::new(WORKTREE)
            .with_mcp_prompts(&prompts)
            .with_skills(&skills),
    );
    let resolved = registry.get("srv:noargs").expect("srv:noargs resolves");

    assert_eq!(
        resolved.source,
        Source::Mcp,
        "observed on the real binary: a skill named srv:noargs vanished"
    );
    assert_eq!(registry.len(), 3);
}

#[test]
fn skill_claims_a_free_name() {
    let skills = [skill("skillonly", "SKILL BODY skillonly")];
    let registry = Registry::build(&Sources::new(WORKTREE).with_skills(&skills));
    let resolved = registry.get("skillonly").expect("skillonly resolves");

    assert_eq!(resolved.source, Source::Skill);
    assert_eq!(
        resolved.description.as_deref(),
        Some("SKILL DESC skillonly")
    );
    assert!(
        resolved.hints.is_empty(),
        "command/index.ts:150 always gives a skill empty hints"
    );
}

#[test]
fn skill_template_gets_a_base_directory_footer() {
    let skills = [skill("skillonly", "SKILL BODY skillonly\n")];
    let registry = Registry::build(&Sources::new(WORKTREE).with_skills(&skills));
    let template = text_of(registry.get("skillonly").expect("skillonly resolves"));

    // Observed on the real binary, verbatim including the three newlines that
    // arise from the body's own trailing newline plus the joined blank line.
    assert_eq!(
        template,
        "SKILL BODY skillonly\n\n\nBase directory for this skill: /skills/skillonly\n\
         Relative paths in this skill (e.g., scripts/, references/) are relative to this base directory."
    );
}

#[test]
fn builtin_skill_gets_no_footer() {
    let skills = [SkillCommand {
        name: "customize-zuno".to_owned(),
        description: Some("built in".to_owned()),
        content: "BODY ONLY".to_owned(),
        location: SkillLocation::Builtin,
    }];
    let registry = Registry::build(&Sources::new(WORKTREE).with_skills(&skills));

    assert_eq!(
        text_of(registry.get("customize-zuno").expect("the skill resolves")),
        "BODY ONLY",
        "command/index.ts:136 skips the footer for the <built-in> sentinel"
    );
}

#[test]
fn skill_body_placeholders_are_not_advertised_but_still_expand() {
    let skills = [skill("noisy", "use $1 and $ARGUMENTS")];
    let registry = Registry::build(&Sources::new(WORKTREE).with_skills(&skills));
    let resolved = registry.get("noisy").expect("noisy resolves");

    assert!(
        resolved.hints.is_empty(),
        "the oracle hardcodes an empty hint list for skills"
    );
    let Resolution::Ready(ready) = registry
        .resolve("noisy", "one two")
        .expect("noisy resolves")
    else {
        panic!("a skill template is ready text");
    };
    assert!(
        ready.prompt.starts_with("use one two and one two"),
        "expansion still runs over the body: {:?}",
        ready.prompt
    );
}

// ---------------------------------------------------------------------------
// The whole chain at once
// ---------------------------------------------------------------------------

#[test]
fn the_full_chain_resolves_in_ascending_precedence() {
    let cfg = config(&[
        ("review", entry("CONFIG REVIEW")),
        ("srv:hello", entry("CONFIG SRV HELLO")),
        ("configonly", entry("CONFIG ONLY")),
    ]);
    let prompts = [prompt("srv", "hello", &["a"])];
    let skills = [
        skill("init", "skill init"),
        skill("review", "skill review"),
        skill("configonly", "skill configonly"),
        skill("srv:hello", "skill srv hello"),
        skill("skillonly", "skill only"),
    ];
    let registry = Registry::build(
        &Sources::new(WORKTREE)
            .with_config(Some(&cfg))
            .with_mcp_prompts(&prompts)
            .with_skills(&skills),
    );

    let winners: Vec<(&str, Source)> = registry
        .list()
        .map(|info| (info.name.as_str(), info.source))
        .collect();
    assert_eq!(
        winners,
        vec![
            ("init", Source::Command),
            ("review", Source::Command),
            ("srv:hello", Source::Mcp),
            ("configonly", Source::Command),
            ("skillonly", Source::Skill),
        ],
        "four skills lost, one claimed a free name"
    );
    assert_eq!(
        text_of(registry.get("review").expect("review resolves")),
        "CONFIG REVIEW",
        "config beat the built-in, and the skill beat nothing"
    );
}

// ---------------------------------------------------------------------------
// Resolution and dispatch
// ---------------------------------------------------------------------------

#[test]
fn resolve_expands_before_dispatch() {
    let cfg = config(&[("greet", entry("Hello $1, meet $2"))]);
    let registry = Registry::build(&Sources::new(WORKTREE).with_config(Some(&cfg)));

    let Resolution::Ready(ready) = registry
        .resolve("greet", "Ada Grace")
        .expect("greet resolves")
    else {
        panic!("a config template is ready text");
    };
    assert_eq!(ready.prompt, "Hello Ada, meet Grace");
    assert_eq!(ready.source, Source::Command);
}

#[test]
fn resolve_carries_agent_model_and_subtask_through() {
    let cfg = config(&[(
        "deep",
        CommandConfig {
            template: "GO".to_owned(),
            description: None,
            agent: Some("explore".to_owned()),
            model: Some("openai/gpt".to_owned()),
            variant: None,
            subtask: Some(true),
        },
    )]);
    let registry = Registry::build(&Sources::new(WORKTREE).with_config(Some(&cfg)));

    let Resolution::Ready(ready) = registry.resolve("deep", "").expect("deep resolves") else {
        panic!("a config template is ready text");
    };
    assert_eq!(ready.agent.as_deref(), Some("explore"));
    assert_eq!(ready.model.as_deref(), Some("openai/gpt"));
    assert_eq!(ready.subtask, Some(true));
}

#[test]
fn resolving_an_unknown_name_names_the_alternatives() {
    let cfg = config(&[("known", entry("T"))]);
    let registry = Registry::build(&Sources::new(WORKTREE).with_config(Some(&cfg)));

    let error = registry
        .resolve("missing", "")
        .expect_err("an unknown command fails");
    let CommandError::NotFound { name, available } = &error;
    assert_eq!(name, "missing");
    assert_eq!(available, &["init", "review", "known"]);
    assert_eq!(
        error.to_string(),
        "Command not found: \"missing\". Available commands: init, review, known",
        "matching the oracle's message at session/prompt.ts:1364-1366"
    );
}

#[test]
fn an_mcp_command_defers_until_the_server_answers() {
    let prompts = [prompt("srv", "hello", &["alpha", "beta"])];
    let registry = Registry::build(&Sources::new(WORKTREE).with_mcp_prompts(&prompts));

    let Resolution::PendingMcp(pending) = registry
        .resolve("srv:hello", "one two three")
        .expect("srv:hello resolves")
    else {
        panic!("an MCP command cannot resolve without a round trip");
    };
    assert_eq!(pending.request.client, "srv");
    assert_eq!(pending.request.prompt, "hello");
    assert_eq!(
        pending.arguments, "one two three",
        "the user's arguments survive unexpanded, because the server's answer is what gets expanded"
    );

    // The server substituted the literal "$1"/"$2" it was handed, so the
    // placeholders are still there for expansion to fill.
    let resolved = pending.complete(&[
        Some("alpha=$1 beta=$2".to_owned()),
        Some("second line".to_owned()),
    ]);
    assert_eq!(
        resolved.prompt, "alpha=one beta=two three\nsecond line",
        "messages join with a newline, then $1/$2 expand with the greedy last rule"
    );
    assert_eq!(resolved.source, Source::Mcp);
}

#[test]
fn non_text_mcp_messages_join_as_empty_strings() {
    assert_eq!(
        join_prompt_messages(&[Some("a".to_owned()), None, Some("b".to_owned())]),
        "a\n\nb",
        "command/index.ts:124 maps a non-text block to \"\" but still joins it"
    );
    assert_eq!(join_prompt_messages(&[]), "");
    assert_eq!(join_prompt_messages(&[None]), "");
}

#[test]
fn an_empty_mcp_result_yields_the_arguments_alone() {
    let prompts = [prompt("srv", "hello", &[])];
    let registry = Registry::build(&Sources::new(WORKTREE).with_mcp_prompts(&prompts));
    let Resolution::PendingMcp(pending) = registry
        .resolve("srv:hello", "user text")
        .expect("srv:hello resolves")
    else {
        panic!("an MCP command defers");
    };

    // An empty template mentions no placeholder, so the append-fallback fires.
    assert_eq!(pending.complete(&[]).prompt, "user text");
}

// ---------------------------------------------------------------------------
// Expansion — the edge cases the plan calls out, plus the ones it does not
// ---------------------------------------------------------------------------

#[test]
fn arguments_placeholder_takes_the_whole_input() {
    assert_eq!(
        expand("Input: $ARGUMENTS", "extra args"),
        "Input: extra args"
    );
}

#[test]
fn arguments_placeholder_with_no_input_leaves_nothing() {
    assert_eq!(expand("Input: $ARGUMENTS", ""), "Input:");
    assert_eq!(
        expand("Input: $ARGUMENTS", "   "),
        "Input:",
        "the trailing trim removes the gap the empty substitution left"
    );
}

#[test]
fn a_positional_past_the_end_substitutes_empty() {
    assert_eq!(
        expand("A=[$1] B=[$2] C=[$3]", "one two"),
        "A=[one] B=[two] C=[]",
        "the plan's failure scenario: $3 with two arguments is empty, not a panic"
    );
}

#[test]
fn the_highest_positional_is_greedy() {
    assert_eq!(
        expand("A=[$1] B=[$2]", "one two three four"),
        "A=[one] B=[two three four]"
    );
    assert_eq!(expand("only: $1", "one two three"), "only: one two three");
    assert_eq!(
        expand("B=[$2] A=[$1]", "one two three"),
        "B=[two three] A=[one]",
        "greediness follows the number, not the position in the text"
    );
}

#[test]
fn a_gap_in_the_numbering_does_not_shift_arguments() {
    assert_eq!(
        expand("A=[$1] C=[$3]", "one two three four"),
        "A=[one] C=[three four]"
    );
}

#[test]
fn zero_renders_undefined_unless_it_is_the_highest() {
    assert_eq!(
        expand("Z=[$0] ONE=[$1]", "one two"),
        "Z=[undefined] ONE=[one two]",
        "args[-1] is undefined, which JavaScript stringifies"
    );
    assert_eq!(
        expand("only: $0", "a b c"),
        "only: c",
        "as the highest placeholder, $0 becomes args.slice(-1) — the last argument"
    );
    assert_eq!(expand("only: $0", ""), "only:");
    assert_eq!(
        expand("Z=[$00] ONE=[$1]", "one two"),
        "Z=[undefined] ONE=[one two]"
    );
}

#[test]
fn a_leading_zero_is_still_that_number() {
    assert_eq!(
        expand("A=[$01] B=[$2]", "one two three"),
        "A=[one] B=[two three]"
    );
}

#[test]
fn a_dollar_amount_is_read_as_a_placeholder() {
    assert_eq!(
        expand("COST IS $5.00 and $x and $", "one two"),
        "COST IS .00 and $x and $",
        "a real trap: $5 matches, so a price in a template loses its digits"
    );
    assert_eq!(hints("COST IS $5.00 and $x and $"), vec!["$5".to_owned()]);
}

#[test]
fn a_non_ascii_digit_is_not_a_placeholder() {
    assert_eq!(expand("$\u{663} and $1", "a b"), "$\u{663} and a b");
}

#[test]
fn an_absurd_placeholder_number_does_not_panic() {
    assert_eq!(expand("$99999999999999999999", "a b"), "");
    assert_eq!(expand("$999", "a b"), "");
}

#[test]
fn a_template_with_no_placeholder_gets_the_input_appended() {
    assert_eq!(
        expand("NO PLACEHOLDERS HERE", "trailing input"),
        "NO PLACEHOLDERS HERE\n\ntrailing input"
    );
    assert_eq!(expand("NO PLACEHOLDERS HERE", ""), "NO PLACEHOLDERS HERE");
    assert_eq!(
        expand("NO PLACEHOLDERS HERE", "  \t "),
        "NO PLACEHOLDERS HERE",
        "a blank input is not appended"
    );
}

#[test]
fn the_append_fallback_does_not_fire_when_any_placeholder_exists() {
    assert_eq!(
        expand("A=[$1]", "one two"),
        "A=[one two]",
        "$1 counts, so nothing is appended"
    );
    assert_eq!(expand("A=[$ARGUMENTS]", "one two"), "A=[one two]");
}

#[test]
fn arguments_keeps_the_raw_input_while_positionals_are_tokenized() {
    assert_eq!(
        expand("P=[$1] ALL=[$ARGUMENTS]", "\"quoted arg\"  spaced"),
        "P=[quoted arg spaced] ALL=[\"quoted arg\"  spaced]",
        "quotes and the double space survive in $ARGUMENTS only"
    );
}

#[test]
fn dollar_patterns_inside_the_input_are_interpreted_for_arguments_only() {
    assert_eq!(
        expand("ALL=[$ARGUMENTS]", "cost $$ high"),
        "ALL=[cost $ high]",
        "$$ collapses because replaceAll took a string replacement"
    );
    assert_eq!(
        expand("ALL=[$ARGUMENTS]", "$& weird"),
        "ALL=[$ARGUMENTS weird]",
        "$& re-inserts the matched placeholder text"
    );
    assert_eq!(
        expand("P=[$1] Q=[$2]", "$& tail"),
        "P=[$&] Q=[tail]",
        "positional substitution uses a function replacer, so $& is literal"
    );
    assert_eq!(
        expand("P=[$1]", "$$ tail"),
        "P=[$$ tail]",
        "and $$ is literal there too"
    );
}

#[test]
fn hints_are_deduplicated_and_sorted_lexicographically() {
    assert_eq!(
        hints("$2 and $10"),
        vec!["$10".to_owned(), "$2".to_owned()],
        "observed on the real binary: the sort is on the string, not the number"
    );
    assert_eq!(
        hints("T=[$10] ONE=[$1]"),
        vec!["$1".to_owned(), "$10".to_owned()]
    );
    assert_eq!(
        hints("$1 and $1 and $2"),
        vec!["$1".to_owned(), "$2".to_owned()],
        "a repeat is listed once"
    );
    assert_eq!(
        hints("A=[$1] ALL=[$ARGUMENTS]"),
        vec!["$1".to_owned(), "$ARGUMENTS".to_owned()],
        "$ARGUMENTS is appended after the sort, never inside it"
    );
    assert!(hints("nothing here").is_empty());
    assert_eq!(hints("$01 and $1"), vec!["$01".to_owned(), "$1".to_owned()]);
}

#[test]
fn tokenizing_follows_the_oracles_regex() {
    assert_eq!(tokenize("one two"), vec!["one", "two"]);
    assert_eq!(tokenize("one    two\tthree"), vec!["one", "two", "three"]);
    assert_eq!(
        tokenize("\"hello world\" second"),
        vec!["hello world", "second"]
    );
    assert_eq!(
        tokenize("'hello world' second"),
        vec!["hello world", "second"]
    );
    assert_eq!(tokenize("[Image 3] caption"), vec!["[Image 3]", "caption"]);
    assert_eq!(
        tokenize("[image 12] tail"),
        vec!["[image 12]", "tail"],
        "the regex carries the i flag"
    );
    assert_eq!(
        tokenize("\" second"),
        vec!["second"],
        "an unpaired quote matches no alternative and is skipped whole"
    );
    assert_eq!(tokenize("\"unclosed second"), vec!["unclosed", "second"]);
    assert_eq!(
        tokenize("\"\" second"),
        vec!["", "second"],
        "an empty quoted run is an empty token"
    );
    assert_eq!(
        tokenize("don't stop"),
        vec!["don", "t", "stop"],
        "the apostrophe opens a group that never closes"
    );
    assert_eq!(tokenize(""), Vec::<String>::new());
    assert_eq!(tokenize("   "), Vec::<String>::new());
    assert_eq!(
        tokenize("\u{65e5}\u{672c}\u{8a9e} \u{30c6}"),
        vec!["\u{65e5}\u{672c}\u{8a9e}", "\u{30c6}"]
    );
}

#[test]
fn an_image_marker_needs_whitespace_and_digits() {
    assert_eq!(
        tokenize("[Image] x"),
        vec!["[Image]", "x"],
        "no digits, so it falls through to the bare run"
    );
    assert_eq!(
        tokenize("[Image3] x"),
        vec!["[Image3]", "x"],
        "no whitespace either"
    );
    assert_eq!(
        tokenize("[Image 3 x"),
        vec!["[Image", "3", "x"],
        "no closing bracket"
    );
}

#[test]
fn expansion_trims_and_survives_a_multiline_template() {
    assert_eq!(expand("   $ARGUMENTS   ", "mid"), "mid");
    assert_eq!(expand("   $1   ", ""), "");
    assert_eq!(
        expand("line1 $1\nline2 $2\nline3", "a b c"),
        "line1 a\nline2 b c\nline3"
    );
    assert_eq!(expand("pre$1post", "VAL"), "preVALpost");
    assert_eq!(expand("x$ARGUMENTSy", "MID"), "xMIDy");
}

#[test]
fn arguments_is_case_sensitive() {
    assert_eq!(
        expand("$arguments and $ARGUMENTS", "up"),
        "$arguments and up"
    );
}

#[test]
fn source_renders_the_oracles_wire_spelling() {
    assert_eq!(Source::Command.to_string(), "command");
    assert_eq!(Source::Mcp.to_string(), "mcp");
    assert_eq!(Source::Skill.to_string(), "skill");
}

#[test]
fn js_replace_all_leaves_capture_references_alone() {
    assert_eq!(
        js_replace_all("a X b", "X", "$1 $<name>"),
        "a $1 $<name> b",
        "there are no capture groups, so these are literal"
    );
    assert_eq!(js_replace_all("a X b", "X", "trailing $"), "a trailing $ b");
    assert_eq!(js_replace_all("no match", "", "ignored"), "no match");
    assert_eq!(js_replace_all("XX", "X", "$&$&"), "XXXX");
}

#[test]
fn a_registry_is_never_empty() {
    let registry = Registry::build(&Sources::new(WORKTREE));
    assert!(!registry.is_empty());
    assert_eq!(registry.len(), 2);
}

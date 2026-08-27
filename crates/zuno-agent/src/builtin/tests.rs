//! The roster's contract, asserted rather than reviewed.
//!
//! The load-bearing test is [`every_agent_states_every_column`]: it iterates the
//! roster instead of a hand-written list, so an agent added without a boundary, a
//! temperature, a deny-by-default set, or an output contract fails here. The
//! exemptions the internals need are expressed in [`Boundary`] and
//! [`OutputContract`] as data, and this module checks that only an internal may
//! claim them — so the exemption cannot be borrowed by a newly added subagent.

use super::*;
use crate::model_policy::looks_like_model_id;
use std::collections::BTreeSet;
use zuno_llm::catalog::resolved::ModalityFlags;
use zuno_permission::visibility::{is_tool_hidden, permission_key};

fn capabilities(image: bool) -> ModelCapabilities {
    ModelCapabilities {
        input: ModalityFlags {
            image,
            ..ModalityFlags::default()
        },
        ..ModelCapabilities::default()
    }
}

/// A tool id no registry will ever contain, used to prove the catch-all bites.
const UNKNOWN_TOOL: &str = "some_tool_added_next_year";

#[test]
fn every_agent_states_every_column() {
    let roster = roster(true);
    assert_eq!(
        roster.len(),
        LEAN_NAMES.len() + INTERNAL_NAMES.len(),
        "roster length must equal the two name lists; update both or neither"
    );

    for agent in &roster {
        let name = agent.name;

        assert!(!name.is_empty(), "an agent must be named");
        assert!(
            !name.contains(char::is_whitespace),
            "{name}: a name is used as a `task` argument, so it cannot contain whitespace"
        );
        assert!(
            agent.description.trim().len() >= 60,
            "{name}: description is {} chars; a positive description that short does not \
             tell a caller when to pick this agent",
            agent.description.trim().len()
        );

        match agent.boundary {
            Boundary::DontDelegateWhen(clause) => {
                assert!(
                    clause.trim().len() >= 40,
                    "{name}: negative boundary is {} chars; \"Don't delegate when…\" must \
                     name real cases or it is decoration",
                    clause.trim().len()
                );
            }
            Boundary::NotDelegable { reason } => {
                match agent.role {
                    Role::Primary => {
                        assert_eq!(
                            name, "build",
                            "{name}: only the direct build primary may be non-delegable"
                        );
                        assert_eq!(agent.delegation, Delegation::NoChildren);
                    }
                    Role::Internal => assert!(
                        INTERNAL_NAMES.contains(&name),
                        "{name}: claims the internal exemption but is not one of the engine's \
                         own agents"
                    ),
                    Role::Orchestrator | Role::Subagent => panic!(
                        "{name}: a routing role must state when the caller should not delegate"
                    ),
                }
                assert!(
                    reason.trim().len() >= 40,
                    "{name}: the exemption needs an argument, not a shrug"
                );
            }
        }
        assert!(
            !agent.boundary.render().is_empty(),
            "{name}: the boundary must render for the build prompt"
        );

        assert!(
            agent.temperature.is_finite(),
            "{name}: temperature must be a real number"
        );
        assert!(
            (0.0..=1.0).contains(&agent.temperature),
            "{name}: temperature {} is outside the sampler's usable range — a typo like \
             10.0 must not survive",
            agent.temperature
        );
        if !matches!(agent.role, Role::Internal) {
            assert!(
                (0.1..=0.5).contains(&agent.temperature),
                "{name}: temperature {} leaves the roster's 0.1-0.2 band by more than the \
                 one deliberate exception allows",
                agent.temperature
            );
        }

        let rules = agent.rules();
        let first = rules.first().expect("a permission set is never empty");
        assert_eq!(
            (
                first.permission.as_str(),
                first.pattern.as_str(),
                first.action
            ),
            ("*", "*", PermissionAction::Deny),
            "{name}: a deny-by-default set opens with the catch-all deny"
        );
        assert!(
            is_tool_hidden(UNKNOWN_TOOL, &rules),
            "{name}: a tool this roster has never heard of must be denied, not inherited"
        );

        match agent.output {
            OutputContract::Natural => {
                assert!(
                    agent.role != Role::Internal,
                    "{name}: an engine-internal completion needs its prompt-owned contract"
                );
            }
            OutputContract::EnginePrompt { prompt } => {
                assert_eq!(
                    agent.role,
                    Role::Internal,
                    "{name}: only an engine-internal agent may expose a raw engine prompt"
                );
                assert!(
                    prompt.len() > 200,
                    "{name}: the exemption points at an upstream prompt, so that prompt \
                     must actually be present"
                );
            }
        }
    }
}

#[test]
fn the_native_nine_are_the_designed_roster_and_nothing_else() {
    let names: Vec<&str> = lean().iter().map(|agent| agent.name).collect();
    assert_eq!(names, LEAN_NAMES.to_vec());
}

#[test]
fn the_dropped_agents_stay_dropped() {
    // Every name here was deliberately cut or replaced: the planner triad and Team
    // Mode (omo), Council's runtime profiles (implemented later as a workflow), the
    // deferred designer lane, and the unpublished advisor/worker names.
    for forbidden in [
        "prometheus",
        "metis",
        "momus",
        "council",
        "councillor",
        "designer",
        "observer",
        "advisor",
        "worker",
        "team",
        "teammode",
    ] {
        assert!(
            get(forbidden, true).is_none(),
            "{forbidden} must not be in the roster"
        );
    }
}

#[test]
fn looker_is_present_exactly_when_a_vision_capable_model_resolves() {
    let with_vision: Vec<&str> = roster(true).iter().map(|agent| agent.name).collect();
    let without_vision: Vec<&str> = roster(false).iter().map(|agent| agent.name).collect();

    assert!(with_vision.contains(&"looker"));
    assert!(!without_vision.contains(&"looker"));
    assert_eq!(
        with_vision.len(),
        without_vision.len() + 1,
        "the vision gate must add exactly one agent and change nothing else"
    );
    assert!(get("looker", true).is_some());
    assert!(get("looker", false).is_none());

    // The gate is a capability question, not a preference: an image-accepting model
    // anywhere in the resolved catalog is enough, and no configuration opts in.
    assert!(any_vision_capable([&capabilities(true)]));
    assert!(any_vision_capable([
        &capabilities(false),
        &capabilities(true)
    ]));
    assert!(!any_vision_capable([
        &capabilities(false),
        &capabilities(false)
    ]));
    assert!(!any_vision_capable(std::iter::empty()));

    assert_eq!(LOOKER.gate, Gate::VisionModel);
    for agent in roster(false) {
        assert_eq!(
            agent.gate,
            Gate::Always,
            "{}: only `looker` is capability-gated",
            agent.name
        );
    }
}

#[test]
fn attachment_support_alone_does_not_make_a_model_vision_capable() {
    // The pinned catalog fixture has models with `attachment: true` whose only input
    // modality is text; treating that flag as the vision signal would put `looker`
    // in the roster for a model that errors on an image.
    let attachment_only = ModelCapabilities {
        attachment: true,
        ..capabilities(false)
    };
    assert!(!is_vision_capable(&attachment_only));

    let image_input = ModelCapabilities {
        attachment: false,
        ..capabilities(true)
    };
    assert!(is_vision_capable(&image_input));
}

/// The scanner now lives in [`crate::model_policy`], which is the module whose whole
/// contract is that no model id exists in this crate; both it and this roster's prose
/// scan call the same definition rather than keeping two that drift.
#[test]
fn the_model_id_scanner_catches_model_ids_and_nothing_else() {
    for positive in [
        "claude-sonnet-4-5",
        "anthropic/claude-3-5-haiku",
        "gpt-5",
        "openai/gpt-4o",
        "gemini-2.0-flash",
        "google/gemini-1.5-pro",
        "o3-mini",
        "moonshotai/kimi-k2",
        "zai/glm-4.6",
        "vendor/model-7b",
    ] {
        assert!(
            looks_like_model_id(positive),
            "{positive} should be recognised as a model id"
        );
    }
    for negative in [
        "src/auth.ts",
        "utils/parser.ts",
        "/path/to/file.rs:42",
        "read-only",
        "temperature",
        "0.1",
        "webfetch",
        "build",
        "and/or",
    ] {
        assert!(
            !looks_like_model_id(negative),
            "{negative} is not a model id"
        );
    }
}

#[test]
fn no_agent_names_a_model() {
    // Every agent inherits the session model (todo 64). A model id in a description,
    // a boundary, or an envelope hint would pin the roster to today's model market
    // just as effectively as a struct field would — and is far easier to miss in
    // review, which is why the prose is scanned and not just the fields.
    for agent in roster(true) {
        for rendered in agent.rendered_strings() {
            for token in rendered.split_whitespace() {
                assert!(
                    !looks_like_model_id(token),
                    "{}: `{token}` looks like a model id, in: {rendered}",
                    agent.name
                );
            }
        }
    }
    for rendered in [render_list(true), render_list(false)] {
        for token in rendered.split_whitespace() {
            assert!(
                !looks_like_model_id(token),
                "`{token}` looks like a model id in the rendered agent list"
            );
        }
    }
}

#[test]
fn only_orchestrator_may_delegate() {
    let may_delegate: Vec<&str> = roster(true)
        .iter()
        .filter(|agent| agent.delegation == Delegation::MayDelegate)
        .map(|agent| agent.name)
        .collect();
    assert_eq!(may_delegate, vec!["orchestrator"]);

    for agent in roster(true) {
        if agent.delegation == Delegation::NoChildren {
            assert!(
                is_tool_hidden("task", &agent.rules()),
                "{}: an agent that may not delegate must not see `task`",
                agent.name
            );
        }
    }
    assert!(!is_tool_hidden("task", &ORCHESTRATOR.rules()));
    assert!(is_tool_hidden("task", &BUILD.rules()));
}

#[test]
fn only_subagents_are_valid_delegation_targets() {
    let targets: Vec<&str> = delegable(true).iter().map(|agent| agent.name).collect();
    assert_eq!(
        targets,
        vec![
            "deep",
            "fixer",
            "general",
            "explorer",
            "librarian",
            "oracle",
            "looker"
        ]
    );
    assert!(!targets.contains(&"build"));
    assert!(!targets.contains(&"orchestrator"));
    for internal in INTERNAL_NAMES {
        assert!(!targets.contains(&internal));
    }
    assert_eq!(delegable(false).len(), 6);
}

#[test]
fn every_delegable_agent_has_a_real_catalog_definition() {
    for agent in delegable(true) {
        let catalog = zuno_catalog::agent::builtin::get(agent.name).unwrap_or_else(|| {
            panic!(
                "task advertises `{}` but no child can resolve it",
                agent.name
            )
        });
        assert_eq!(catalog.mode, AgentMode::Subagent, "{}", agent.name);
        assert!(
            catalog
                .prompt
                .is_some_and(|prompt| !prompt.trim().is_empty()),
            "{} needs an executable prompt",
            agent.name
        );
    }
}

#[test]
fn deep_owns_cross_cutting_implementation_without_children() {
    assert_eq!(DEEP.write, Write::Capable);
    assert_eq!(DEEP.research, Research::Allowed);
    assert_eq!(DEEP.delegation, Delegation::NoChildren);
    assert!(is_tool_hidden("task", &DEEP.rules()));
    for capability in ["edit", "shell", "web_search", "plan_update", "todo_update"] {
        assert!(
            !is_tool_hidden(capability, &DEEP.rules()),
            "deep needs `{capability}`"
        );
    }
}

#[test]
fn fixer_is_a_focused_writer_and_general_is_the_bounded_fallback() {
    assert_eq!(FIXER.write, Write::Capable);
    assert_eq!(FIXER.research, Research::Confined);
    assert_eq!(FIXER.delegation, Delegation::NoChildren);

    let fixer = FIXER.rules();
    for allowed in ["read", "grep", "glob", "lsp", "edit", "shell", "skill"] {
        assert!(
            !is_tool_hidden(allowed, &fixer),
            "the fixer needs `{allowed}` to inspect, edit, and verify"
        );
    }
    for forbidden in ["task", "webfetch", "web_search", "execute"] {
        assert!(
            is_tool_hidden(forbidden, &fixer),
            "fixer sees `{forbidden}`"
        );
    }

    assert_eq!(GENERAL.write, Write::Capable);
    assert_eq!(GENERAL.research, Research::Allowed);
    assert_eq!(GENERAL.delegation, Delegation::NoChildren);
    let general = GENERAL.rules();
    for allowed in ["read", "edit", "shell", "web_search", "skill", "execute"] {
        assert!(
            !is_tool_hidden(allowed, &general),
            "general needs bounded access to `{allowed}`"
        );
    }
    assert!(is_tool_hidden("task", &general));

    // Editing is one permission key, so granting `edit` grants the whole family and
    // there is no way to allow one alias while denying another.
    for alias in ["write", "apply_patch"] {
        assert_eq!(permission_key(alias), "edit");
        assert!(!is_tool_hidden(alias, &fixer));
    }
}

#[test]
fn read_only_agents_get_shell_without_write_or_delegation_authority() {
    for agent in roster(true) {
        if agent.write != Write::ReadOnly || agent.role == Role::Internal {
            continue;
        }
        let rules = agent.rules();
        assert!(
            !is_tool_hidden("shell", &rules),
            "{}: the OS sandbox makes Shell read-only for this Agent",
            agent.name
        );
        for forbidden in ["edit", "write", "apply_patch", "task", "execute"] {
            assert!(
                is_tool_hidden(forbidden, &rules),
                "{}: read-only agents must not see `{forbidden}`",
                agent.name
            );
        }
    }
}

#[test]
fn every_delegable_agent_can_load_skills() {
    for agent in delegable(true) {
        assert!(
            !is_tool_hidden("skill", &agent.rules()),
            "{} must be able to load an explicitly available Skill",
            agent.name
        );
    }
}

#[test]
fn external_research_is_the_librarians_lane_alone() {
    for agent in roster(true) {
        if agent.name == "librarian" || agent.write == Write::Capable {
            continue;
        }
        let rules = agent.rules();
        for external in ["webfetch", "web_search"] {
            assert!(
                is_tool_hidden(external, &rules),
                "{}: only the librarian and the writing agents reach `{external}`",
                agent.name
            );
        }
    }
    let rules = LIBRARIAN.rules();
    assert!(!is_tool_hidden("webfetch", &rules));
    assert!(!is_tool_hidden("web_search", &rules));
}

#[test]
fn every_permission_set_gives_every_governed_tool_an_explicit_verdict() {
    let governed: BTreeSet<&str> = GOVERNED_TOOL_IDS.iter().copied().collect();
    assert_eq!(
        governed.len(),
        GOVERNED_TOOL_IDS.len(),
        "the governed tool list must not repeat an id"
    );

    for agent in roster(true) {
        let name = agent.name;
        let denied: BTreeSet<&str> = agent.permissions.denied.iter().copied().collect();
        let allowed: BTreeSet<&str> = agent.permissions.allowed.iter().copied().collect();

        assert!(
            denied.is_disjoint(&allowed),
            "{name}: {:?} is both denied and allowed",
            denied.intersection(&allowed).collect::<Vec<_>>()
        );
        for tool in denied.union(&allowed) {
            assert!(
                governed.contains(tool),
                "{name}: `{tool}` is not a governed tool id, so the rule is dead config"
            );
            assert_eq!(
                permission_key(tool),
                *tool,
                "{name}: `{tool}` is an alias; name the permission key it collapses to"
            );
        }
        let stated: BTreeSet<&str> = denied.union(&allowed).copied().collect();
        assert_eq!(
            stated,
            governed,
            "{name}: every governed tool needs a stated verdict; missing {:?}",
            governed.difference(&stated).collect::<Vec<_>>()
        );
    }
}

#[test]
fn the_allows_survive_the_catch_all_deny() {
    // `zuno_permission` takes the *last* matching rule, so emitting allows before the
    // wildcard deny would produce a set that reads as an allow-list and behaves as a
    // deny-all. Order is therefore part of the contract.
    let rules = EXPLORER.rules();
    for allowed in READ_ONLY_ALLOWED {
        assert_eq!(
            zuno_permission::evaluate(allowed, "anything", &rules),
            PermissionAction::Allow,
            "`{allowed}` must survive the catch-all"
        );
    }
    for denied in READ_ONLY_DENIED {
        assert_eq!(
            zuno_permission::evaluate(denied, "anything", &rules),
            PermissionAction::Deny
        );
    }
    assert_eq!(
        zuno_permission::evaluate(UNKNOWN_TOOL, "anything", &rules),
        PermissionAction::Deny
    );

    let positions: Vec<usize> = rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.action == PermissionAction::Allow)
        .map(|(index, _)| index)
        .collect();
    let wildcard = rules
        .iter()
        .position(|rule| rule.permission == "*")
        .expect("the catch-all is present");
    assert!(
        positions.iter().all(|index| *index > wildcard),
        "every allow must come after the catch-all deny"
    );
}

#[test]
fn work_capable_agents_inherit_extension_tools_from_the_parent_authority() {
    let mcp = ["playwright_navigate", "github_create_issue"];

    for primary in [ORCHESTRATOR, BUILD, DEEP, GENERAL] {
        let rules = primary.rules_with_extension_tools(&mcp);
        for tool in mcp {
            assert!(
                !is_tool_hidden(tool, &rules),
                "{} must see a server the user configured",
                primary.name
            );
        }
        assert!(
            is_tool_hidden(UNKNOWN_TOOL, &rules),
            "deny-by-default survives: only named extension tools are allowed"
        );
    }

    for agent in roster(true) {
        if agent.permissions.extension_tools == ExtensionTools::Inherit {
            assert!(["orchestrator", "build", "deep", "general"].contains(&agent.name));
            continue;
        }
        let rules = agent.rules_with_extension_tools(&mcp);
        assert_eq!(rules, agent.rules(), "{}: unchanged", agent.name);
        for tool in mcp {
            assert!(is_tool_hidden(tool, &rules), "{}: hidden", agent.name);
        }
    }
}

#[test]
fn oracle_is_the_only_agent_above_the_low_band() {
    let above: Vec<(&str, f64)> = lean()
        .iter()
        .filter(|agent| agent.temperature > 0.2)
        .map(|agent| (agent.name, agent.temperature))
        .collect();
    assert_eq!(
        above,
        vec![("oracle", 0.4)],
        "exactly one agent spends temperature, and it is the one that has to disagree"
    );

    assert_eq!(ORACLE.output, OutputContract::Natural);
    let prompt = zuno_catalog::agent::builtin::get("oracle")
        .and_then(|agent| agent.prompt)
        .expect("the oracle prompt");
    assert!(
        prompt.contains("compare at least two viable options") && prompt.contains("recommend one"),
        "the higher-temperature review lane must still require alternatives and a decision"
    );
}

#[test]
fn the_internals_are_the_engines_hidden_agents_carried_unchanged() {
    let internals = internals();
    let names: Vec<&str> = internals.iter().map(|agent| agent.name).collect();
    assert_eq!(names, INTERNAL_NAMES.to_vec());

    for agent in &internals {
        let native = zuno_catalog::agent::builtin::get(agent.name)
            .expect("every internal is one of the engine's natives");
        assert!(
            native.hidden,
            "{}: only a hidden native is an engine internal",
            agent.name
        );
        assert!(agent.hidden, "{}: stays hidden here too", agent.name);
        assert_eq!(agent.mode, native.mode);
        assert_eq!(agent.role, Role::Internal);
        assert_eq!(
            agent.output,
            OutputContract::EnginePrompt {
                prompt: native.prompt.expect("an internal is prompt-driven"),
            },
            "{}: the prompt must be the catalog's, not a copy",
            agent.name
        );
        assert!(
            agent.permissions.allowed.is_empty(),
            "{}: the engine's agents call no tools",
            agent.name
        );
        for tool in GOVERNED_TOOL_IDS {
            assert!(is_tool_hidden(tool, &agent.rules()));
        }
    }

    // Upstream sets a temperature only for `title`; the other two inherit it from
    // this roster's requirement that every agent declare one.
    let by_name = |name: &str| {
        internals
            .iter()
            .find(|agent| agent.name == name)
            .map(|agent| agent.temperature)
    };
    assert_eq!(by_name("title"), Some(0.5));
    assert_eq!(by_name("compaction"), Some(0.1));
    assert_eq!(by_name("summary"), Some(0.1));
}

#[test]
fn plan_mode_is_not_reproduced_and_no_agent_can_leave_it() {
    // `plan` is the fourth upstream native this roster does not carry; see
    // `internals`'s doc comment for the reasoning. The observable consequence is
    // asserted here so a later todo that promotes this roster to the only source of
    // agents trips over it.
    assert!(get("plan", true).is_none());
    assert!(zuno_catalog::agent::builtin::get("plan").is_some());
    for agent in roster(true) {
        assert!(
            is_tool_hidden("plan_exit", &agent.rules()),
            "{}: plan mode is the catalog's native, so nobody here exits it",
            agent.name
        );
    }
}

#[test]
fn user_facing_agents_use_natural_output_without_harness_markup() {
    for agent in lean() {
        assert_eq!(
            agent.output,
            OutputContract::Natural,
            "{}: user-facing output must remain ordinary Markdown",
            agent.name
        );
        let policy = agent.prompt_policy();
        assert!(
            !policy.contains("Output Format")
                && !policy.contains("<orchestration>")
                && !policy.contains("<results>"),
            "{}: harness-only markup leaked into the model prompt: {policy}",
            agent.name
        );
    }
}

#[test]
fn build_prompt_does_not_turn_self_contained_reasoning_into_shell_work() {
    let prompt = zuno_catalog::agent::builtin::get("build")
        .and_then(|agent| agent.prompt)
        .expect("the build prompt");
    for required in [
        "self-contained reasoning or writing question should be answered directly",
        "do not call the shell",
        "throwaway files",
        "Use natural Markdown",
    ] {
        assert!(
            prompt.contains(required),
            "the build prompt lost its deliberate-tool boundary: missing `{required}`"
        );
    }
}

#[test]
fn the_rendered_list_shows_the_nine_with_their_boundaries() {
    let listed = render_list(true);
    for name in LEAN_NAMES {
        assert!(listed.contains(name), "{name} must appear in `agent list`");
        let agent = get(name, true).expect("in the roster");
        assert!(
            listed.contains(&agent.boundary.render()),
            "{name}: its boundary is the point of listing it"
        );
    }
    for internal in INTERNAL_NAMES {
        assert!(
            !listed.contains(internal),
            "{internal} is hidden and must not be listed"
        );
    }
    assert_eq!(
        listed.lines().filter(|line| line.contains("temp=")).count(),
        LEAN_NAMES.len()
    );
    assert!(!render_list(false).contains("looker"));
}

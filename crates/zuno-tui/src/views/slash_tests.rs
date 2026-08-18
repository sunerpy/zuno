use super::*;

fn route(input: &str) -> SlashSubmission {
    SlashRouter::default().resolve(input)
}

#[test]
fn every_ui_route_is_backed_by_the_keybind_source_of_truth() {
    let commands = ui_commands();
    assert_eq!(commands.len(), UI_SPECS.len());
    for command in commands {
        let SlashCommandKind::UiAction(action) = command.kind else {
            panic!("UI projection produced a catalog command");
        };
        let definition = DEFINITIONS
            .iter()
            .find(|definition| definition.name == action)
            .expect("slash action must exist in DEFINITIONS");
        assert_eq!(command.description, definition.description);
    }
}

#[test]
fn every_ui_slash_action_lives_in_a_session_scope() {
    let scopes = crate::views::session::scopes();
    for command in ui_commands() {
        let SlashCommandKind::UiAction(action) = command.kind else {
            panic!("UI projection produced a catalog command");
        };
        let definition = DEFINITIONS
            .iter()
            .find(|definition| definition.name == action)
            .expect("slash action must exist in DEFINITIONS");
        assert!(
            scopes.iter().any(|scope| scope == definition.scope),
            "/{} dispatches `{action}` from unregistered scope `{}`",
            command.name,
            definition.scope
        );
    }
}

#[test]
fn supported_ui_commands_and_compatibility_aliases_dispatch_actions() {
    for (input, action) in [
        ("/models", "model_list"),
        ("/model", "model_list"),
        ("/mo", "model_list"),
        ("/agents", "agent_list"),
        ("/agent", "agent_list"),
        ("/mcps", "mcp_list"),
        ("/mcp", "mcp_list"),
        ("/skills", "prompt_skills"),
        ("/skill", "prompt_skills"),
        ("/sessions", "session_list"),
        ("/session", "session_list"),
        ("/resume", "session_list"),
        ("/continue", "session_list"),
        ("/diff", "diff_open"),
        ("/themes", "theme_list"),
        ("/theme", "theme_list"),
        ("/help", "help_show"),
        ("/commands", "command_list"),
        ("/editor", "editor_open"),
        ("/thinking", "display_thinking"),
        ("/toggle-thinking", "display_thinking"),
        ("/exit", "app_exit"),
        ("/quit", "app_exit"),
        ("/q", "app_exit"),
    ] {
        assert_eq!(route(input), SlashSubmission::UiAction(action), "{input}");
    }
}

#[test]
fn list_actions_accept_natural_singular_and_plural_spellings() {
    for (singular, plural, action) in [
        ("/model", "/models", "model_list"),
        ("/agent", "/agents", "agent_list"),
        ("/mcp", "/mcps", "mcp_list"),
        ("/session", "/sessions", "session_list"),
        ("/theme", "/themes", "theme_list"),
    ] {
        assert_eq!(route(singular), SlashSubmission::UiAction(action));
        assert_eq!(route(plural), SlashSubmission::UiAction(action));
    }
}

#[test]
fn natural_singular_words_are_canonical_for_resource_pickers() {
    let router = SlashRouter::default();
    for (name, action) in [
        ("model", "model_list"),
        ("agent", "agent_list"),
        ("mcp", "mcp_list"),
        ("session", "session_list"),
        ("theme", "theme_list"),
    ] {
        let command = router
            .commands()
            .iter()
            .find(|command| command.kind == SlashCommandKind::UiAction(action))
            .unwrap_or_else(|| panic!("`{action}` is absent from the slash table"));
        assert_eq!(
            command.name, name,
            "`{action}` has a mechanical canonical name"
        );
    }
    assert_eq!(
        route("/commands"),
        SlashSubmission::UiAction("command_list")
    );
}

#[test]
fn an_unconsumed_variant_action_is_not_advertised() {
    for input in ["/variant", "/variants"] {
        assert_eq!(
            route(input),
            SlashSubmission::Unknown(input.trim_start_matches('/').to_owned()),
            "a slash entry was registered for an action the session screen does not consume"
        );
    }
}

#[test]
fn unsupported_command_families_never_enter_the_merged_slash_table() {
    let forbidden = [
        "share",
        "unshare",
        "console-org",
        "org",
        "connect",
        "github-app",
        "workspace-list",
        "warp",
        "move",
        "move-session",
        "session-move",
        "stash",
        "stash-list",
    ];
    let router = SlashRouter::new(
        forbidden
            .iter()
            .map(|name| CatalogCommand::new(*name, Some(format!("forbidden {name}")))),
    );
    let offered = router
        .commands()
        .iter()
        .flat_map(|command| {
            std::iter::once(command.name.as_str()).chain(command.aliases.iter().map(String::as_str))
        })
        .collect::<Vec<_>>();

    for name in forbidden {
        assert!(
            !offered.contains(&name),
            "`/{name}` leaked into {offered:?}"
        );
        assert_eq!(
            router.resolve(&format!("/{name}")),
            SlashSubmission::Unknown(name.to_owned()),
            "`/{name}` resolves despite being outside the product boundary"
        );
    }
}

#[test]
fn catalog_commands_keep_their_unexpanded_arguments() {
    let router = SlashRouter::new([CatalogCommand::new(
        "review",
        Some("Review a change".to_owned()),
    )]);
    assert_eq!(
        router.resolve("/review   src/lib.rs carefully"),
        SlashSubmission::Catalog {
            command: "review".to_owned(),
            arguments: "src/lib.rs carefully".to_owned(),
        }
    );
    let command = router
        .commands()
        .iter()
        .find(|command| command.name == "review")
        .expect("catalog command should be discoverable");
    assert_eq!(command.description, "Review a change");
}

#[test]
fn ui_actions_win_catalog_name_and_alias_collisions() {
    let router = SlashRouter::new([
        CatalogCommand::new("models", None),
        CatalogCommand::new("model", None),
        CatalogCommand::new("help", None),
    ]);
    assert_eq!(
        router.resolve("/models"),
        SlashSubmission::UiAction("model_list")
    );
    assert_eq!(
        router.resolve("/model"),
        SlashSubmission::UiAction("model_list")
    );
    assert_eq!(
        router.resolve("/help"),
        SlashSubmission::UiAction("help_show")
    );
    assert_eq!(
        router
            .commands()
            .iter()
            .filter(|command| command.name == "model")
            .count(),
        1
    );
    assert!(
        !router
            .commands()
            .iter()
            .any(|command| command.name == "models")
    );
}

#[test]
fn unknown_slash_input_is_not_model_input() {
    assert_eq!(
        route("/definitely-unknown argument"),
        SlashSubmission::Unknown("definitely-unknown".to_owned())
    );
    assert_eq!(route("/"), SlashSubmission::Unknown(String::new()));
}

#[test]
fn doubled_slash_is_the_literal_prompt_escape() {
    assert_eq!(
        route("//review this literally"),
        SlashSubmission::Prompt("/review this literally".to_owned())
    );
    assert_eq!(
        route("///path"),
        SlashSubmission::Prompt("//path".to_owned())
    );
    assert_eq!(
        route("ordinary prompt"),
        SlashSubmission::Prompt("ordinary prompt".to_owned())
    );
}

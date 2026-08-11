use std::str::FromStr;

use oc_plugin::{HookName, PluginManifest, hook_support};

const ORACLE_HOOKS: [&str; 21] = [
    "dispose",
    "event",
    "config",
    "tool",
    "auth",
    "provider",
    "chat.message",
    "chat.params",
    "chat.headers",
    "permission.ask",
    "command.execute.before",
    "tool.execute.before",
    "shell.env",
    "tool.execute.after",
    "experimental.chat.messages.transform",
    "experimental.chat.system.transform",
    "experimental.provider.small_model",
    "experimental.session.compacting",
    "experimental.compaction.autocontinue",
    "experimental.text.complete",
    "tool.definition",
];

#[test]
fn manifest_accepts_every_authoritative_hook_when_all_names_are_known() {
    // Given
    let hooks = ORACLE_HOOKS
        .iter()
        .map(|name| HookName::from_str(name).expect("oracle hook must parse"))
        .collect();

    // When
    let manifest = PluginManifest::new("recording", hooks).expect("valid manifest");

    // Then
    assert_eq!(HookName::ALL.map(HookName::as_str), ORACLE_HOOKS);
    assert_eq!(manifest.hooks().len(), ORACLE_HOOKS.len());
}

#[test]
fn manifest_rejects_an_unknown_hook_with_the_valid_hook_list() {
    // Given / When
    let error = HookName::from_str("chat.magic").expect_err("unknown hook must fail");

    // Then
    let rendered = error.to_string();
    assert!(rendered.contains("chat.magic"));
    for hook in ORACLE_HOOKS {
        assert!(rendered.contains(hook), "missing valid hook {hook}");
    }
}

#[test]
fn production_support_matrix_covers_every_advertised_hook_in_declaration_order() {
    let support = hook_support().collect::<Vec<_>>();

    assert_eq!(
        support.iter().map(|row| row.hook).collect::<Vec<_>>(),
        HookName::ALL
    );
    assert!(
        support
            .iter()
            .all(|row| !row.production_trigger.trim().is_empty())
    );
}

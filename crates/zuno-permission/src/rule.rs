use crate::types::Rule;
use crate::wildcard::wildcard_match;
use zuno_config::schema::permission::{
    PermissionAction, PermissionConfig, PermissionObject, PermissionRule,
};

/// Flatten permission configuration without changing either object order.
#[must_use]
pub fn rules_from_config(config: &PermissionConfig) -> Vec<Rule> {
    rules_from_object(&config.rules)
}

fn rules_from_object(object: &PermissionObject) -> Vec<Rule> {
    let mut rules = Vec::new();
    for (permission, configured) in object.iter() {
        match configured {
            PermissionRule::Action(action) => rules.push(Rule {
                permission: permission.to_owned(),
                pattern: "*".to_owned(),
                action: *action,
            }),
            PermissionRule::Patterns(patterns) => {
                rules.extend(patterns.iter().map(|(pattern, action)| Rule {
                    permission: permission.to_owned(),
                    pattern: expand_home(pattern),
                    action: *action,
                }));
            }
        }
    }
    rules
}

/// Return the action from the last rule whose key and value patterns match.
///
/// No matching rule is an ask, never an implicit allow.
#[must_use]
pub fn evaluate(permission: &str, pattern: &str, rules: &[Rule]) -> PermissionAction {
    evaluate_ordered(permission, pattern, rules.iter())
}

pub(crate) fn evaluate_ordered<'a>(
    permission: &str,
    pattern: &str,
    rules: impl DoubleEndedIterator<Item = &'a Rule>,
) -> PermissionAction {
    rules
        .rev()
        .find(|rule| {
            wildcard_match(permission, &rule.permission) && wildcard_match(pattern, &rule.pattern)
        })
        .map_or(PermissionAction::Ask, |rule| rule.action)
}

fn expand_home(pattern: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return pattern.to_owned();
    };
    let home = home.to_string_lossy();
    if let Some(rest) = pattern.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    if pattern == "~" {
        return home.into_owned();
    }
    pattern
        .strip_prefix("$HOME")
        .map_or_else(|| pattern.to_owned(), |rest| format!("{home}{rest}"))
}

use crate::resource::Spellings;
use crate::types::Rule;
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
/// No matching rule is an ask, never an implicit allow. `pattern` is the resource
/// the call names, and it is matched under every spelling
/// [`crate::resource`] accepts for it, so a rule cannot be sidestepped by
/// respelling the command or the path.
#[must_use]
pub fn evaluate(permission: &str, pattern: &str, rules: &[Rule]) -> PermissionAction {
    evaluate_ordered(permission, pattern, rules.iter())
}

pub(crate) fn evaluate_ordered<'a>(
    permission: &str,
    pattern: &str,
    rules: impl DoubleEndedIterator<Item = &'a Rule>,
) -> PermissionAction {
    let spellings = Spellings::new(permission, pattern);
    rules
        .rev()
        .find(|rule| spellings.matches(rule))
        .map_or(PermissionAction::Ask, |rule| rule.action)
}

/// Expand a leading `~` or `$HOME` in a configured pattern.
///
/// The prefix has to end at a path boundary. `$HOMEBREW/*` is a pattern about
/// Homebrew, not about the home directory, and rewriting it silently pointed the
/// rule at a path the user never wrote.
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
        .filter(|rest| starts_at_boundary(rest))
        .map_or_else(|| pattern.to_owned(), |rest| format!("{home}{rest}"))
}

/// Whether what follows an expanded prefix is a new path segment or nothing.
fn starts_at_boundary(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')
}

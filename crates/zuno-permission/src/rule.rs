use crate::resource::{MatchReason, Spellings};
use crate::types::Rule;
use std::fmt;
use zuno_config::schema::permission::{
    PermissionAction, PermissionConfig, PermissionObject, PermissionRule,
};
use zuno_error::ToolError;

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

/// The outcome of matching one resource against a ruleset, with its provenance.
///
/// `action` is what [`evaluate`] returns; `matched` says which rule decided it and
/// under which reading, which is what makes a terminal `deny` explainable to the
/// person who hit it. `None` means no rule matched, which is an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision<'a> {
    pub action: PermissionAction,
    pub matched: Option<Matched<'a>>,
}

/// The rule that decided a [`Decision`], and why it applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched<'a> {
    pub rule: &'a Rule,
    pub reason: MatchReason,
}

impl Decision<'_> {
    /// The owned account of this decision when it is a `deny`, ready to report.
    #[must_use]
    pub fn denial(&self, permission: &str, resource: &str) -> Option<Denial> {
        let matched = self.matched.as_ref()?;
        (matched.rule.action == PermissionAction::Deny).then(|| Denial {
            permission: permission.to_owned(),
            resource: resource.to_owned(),
            rule: matched.rule.clone(),
            reason: matched.reason.clone(),
        })
    }
}

/// A configured `deny` that stopped a request: the rule and the reading it fired
/// under, in the words a user can act on.
///
/// A configured deny is terminal — no prompt follows and no runtime grant can cross
/// it — so a refusal that names only the tool leaves the user guessing which rule
/// and why, most of all when the reading is one this crate applies to a deny alone
/// (a bare `$EDITOR` under `rm -rf*`). [`ToolError::Denied`] carries only the tool,
/// so this converts into it losslessly for the error channel and keeps the account
/// for whoever renders the refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// The permission key the request was made under.
    pub permission: String,
    /// The resource as the request named it.
    pub resource: String,
    /// The `deny` rule that matched.
    pub rule: Rule,
    /// The reading under which it matched.
    pub reason: MatchReason,
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:?} is denied by the {} rule {:?}: {}",
            self.permission, self.resource, self.rule.permission, self.rule.pattern, self.reason
        )
    }
}

impl std::error::Error for Denial {}

impl From<Denial> for ToolError {
    fn from(denial: Denial) -> Self {
        Self::Denied {
            tool: denial.permission,
        }
    }
}

impl From<Box<Denial>> for ToolError {
    fn from(denial: Box<Denial>) -> Self {
        Self::from(*denial)
    }
}

/// Return the action from the last rule whose key and value patterns match.
///
/// No matching rule is an ask, never an implicit allow. `pattern` is the resource
/// the call names, and it is matched under every spelling
/// [`crate::resource`] accepts for it, so a rule cannot be sidestepped by
/// respelling the command or the path. [`decide`] returns the same answer together
/// with the rule and the reading that produced it.
#[must_use]
pub fn evaluate(permission: &str, pattern: &str, rules: &[Rule]) -> PermissionAction {
    decide(permission, pattern, rules).action
}

/// [`evaluate`], keeping which rule decided and why.
#[must_use]
pub fn decide<'a>(permission: &str, pattern: &str, rules: &'a [Rule]) -> Decision<'a> {
    decide_ordered(permission, pattern, rules.iter())
}

pub(crate) fn decide_ordered<'a>(
    permission: &str,
    pattern: &str,
    rules: impl DoubleEndedIterator<Item = &'a Rule>,
) -> Decision<'a> {
    let spellings = Spellings::new(permission, pattern);
    let matched = rules.rev().find_map(|rule| {
        spellings
            .match_reason(rule)
            .map(|reason| Matched { rule, reason })
    });
    Decision {
        action: matched
            .as_ref()
            .map_or(PermissionAction::Ask, |matched| matched.rule.action),
        matched,
    }
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

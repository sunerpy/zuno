use crate::rule::{evaluate, evaluate_ordered};
use crate::types::{
    Authorization, PermissionReply, PermissionRequest, ReplyKind, ReplyOutcome, ResolvedRequest,
    Rule,
};
use zuno_config::schema::permission::PermissionAction;
use zuno_error::ToolError;

#[derive(Debug, Default)]
pub struct PermissionEngine {
    pending: Vec<PermissionRequest>,
    approved: Vec<Rule>,
}

impl PermissionEngine {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            approved: Vec::new(),
        }
    }

    #[must_use]
    pub fn pending(&self) -> &[PermissionRequest] {
        &self.pending
    }

    #[must_use]
    pub fn approved_rules(&self) -> &[Rule] {
        &self.approved
    }

    /// Evaluate a request and either authorize, deny, or store it as pending.
    ///
    /// Runtime approvals are evaluated after the supplied ruleset, so a later
    /// "always" answer can settle what configuration left at `ask`. It can never
    /// settle what configuration denied: `ruleset` is evaluated on its own first
    /// and a `deny` there is terminal, however many runtime grants follow it.
    /// Because that check runs on every call, an installed grant cannot outlive
    /// or outrank the configured prohibition either.
    ///
    /// A request that names no pattern is a request for the resource the rules
    /// spell `*`. It is normalized to that pattern rather than authorized
    /// silently, so an empty pattern list still needs a grant.
    ///
    /// # Errors
    /// Returns [`ToolError::Denied`] as soon as any requested pattern evaluates
    /// to deny. A denied request is never inserted into pending state.
    pub fn authorize(
        &mut self,
        mut request: PermissionRequest,
        ruleset: &[Rule],
    ) -> Result<Authorization, ToolError> {
        if request.patterns.is_empty() {
            request.patterns.push("*".to_owned());
        }
        let mut needs_ask = false;
        for pattern in &request.patterns {
            if evaluate(&request.permission, pattern, ruleset) == PermissionAction::Deny {
                return Err(ToolError::Denied {
                    tool: request.permission.clone(),
                });
            }
            match evaluate_ordered(
                &request.permission,
                pattern,
                ruleset.iter().chain(self.approved.iter()),
            ) {
                PermissionAction::Ask => needs_ask = true,
                PermissionAction::Allow => {}
                PermissionAction::Deny => {
                    return Err(ToolError::Denied {
                        tool: request.permission.clone(),
                    });
                }
            }
        }

        if needs_ask {
            self.pending.push(request);
            Ok(Authorization::Pending)
        } else {
            Ok(Authorization::Allowed)
        }
    }

    /// Apply a reply and return every pending request it resolves.
    ///
    /// Returns `None` without changing state when the request ID is not pending.
    /// Reject resolves every sibling in the same session. Always installs allow
    /// rules from the target's `always` patterns, then resolves same-session
    /// siblings whose complete pattern list those runtime rules cover.
    pub fn reply(&mut self, input: PermissionReply) -> Option<ReplyOutcome> {
        let position = self
            .pending
            .iter()
            .position(|request| request.id == input.request_id)?;
        let target = self.pending.remove(position);

        match input.reply {
            ReplyKind::Reject => Some(self.reject(target, input.message)),
            ReplyKind::Once => Some(ReplyOutcome {
                resolved: vec![ResolvedRequest {
                    request: target,
                    reply: ReplyKind::Once,
                    message: None,
                }],
                installed_rules: Vec::new(),
            }),
            ReplyKind::Always => Some(self.always(target)),
        }
    }

    fn reject(&mut self, target: PermissionRequest, message: Option<String>) -> ReplyOutcome {
        let session_id = target.session_id.clone();
        let mut resolved = vec![ResolvedRequest {
            request: target,
            reply: ReplyKind::Reject,
            message,
        }];
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].session_id == session_id {
                resolved.push(ResolvedRequest {
                    request: self.pending.remove(index),
                    reply: ReplyKind::Reject,
                    message: None,
                });
            } else {
                index += 1;
            }
        }
        ReplyOutcome {
            resolved,
            installed_rules: Vec::new(),
        }
    }

    fn always(&mut self, target: PermissionRequest) -> ReplyOutcome {
        let installed_rules: Vec<_> = target
            .always
            .iter()
            .map(|pattern| Rule {
                permission: target.permission.clone(),
                pattern: pattern.clone(),
                action: PermissionAction::Allow,
            })
            .collect();
        self.approved.extend(installed_rules.iter().cloned());

        let session_id = target.session_id.clone();
        let mut resolved = vec![ResolvedRequest {
            request: target,
            reply: ReplyKind::Always,
            message: None,
        }];
        let mut index = 0;
        while index < self.pending.len() {
            let request = &self.pending[index];
            let covered = request.session_id == session_id
                && request.patterns.iter().all(|pattern| {
                    evaluate(&request.permission, pattern, &self.approved)
                        == PermissionAction::Allow
                });
            if covered {
                resolved.push(ResolvedRequest {
                    request: self.pending.remove(index),
                    reply: ReplyKind::Always,
                    message: None,
                });
            } else {
                index += 1;
            }
        }
        ReplyOutcome {
            resolved,
            installed_rules,
        }
    }
}

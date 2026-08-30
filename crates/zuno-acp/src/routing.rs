//! Maps durable child-session identity onto the ACP session a client knows.

use std::sync::OnceLock;

/// Shared routing policy for permissions and elicitation raised by child turns.
#[derive(Debug)]
pub struct AcpSessionRoute {
    root_session_id: OnceLock<String>,
    native_subagents: bool,
}

impl AcpSessionRoute {
    #[must_use]
    pub fn new(native_subagents: bool) -> Self {
        Self {
            root_session_id: OnceLock::new(),
            native_subagents,
        }
    }

    /// Bind the durable root after the host has resolved or created it.
    pub fn bind_root(&self, session_id: &str) -> Result<(), String> {
        if session_id.is_empty() {
            return Err("an ACP root session id cannot be empty".to_owned());
        }
        match self.root_session_id.set(session_id.to_owned()) {
            Ok(()) => Ok(()),
            Err(_)
                if self
                    .root_session_id
                    .get()
                    .is_some_and(|root| root == session_id) =>
            {
                Ok(())
            }
            Err(_) => Err(format!(
                "ACP session routing is already bound to {}, not {session_id}",
                self.root_session_id
                    .get()
                    .map(String::as_str)
                    .unwrap_or("<unknown>")
            )),
        }
    }

    #[must_use]
    pub fn resolve(&self, actual_session_id: &str) -> RoutedSession {
        let root = self.root_session_id.get().map(String::as_str);
        if self.native_subagents || root.is_none_or(|root| root == actual_session_id) {
            return RoutedSession {
                wire_session_id: actual_session_id.to_owned(),
                child_session_id: None,
                grant_session_id: root.unwrap_or(actual_session_id).to_owned(),
            };
        }
        RoutedSession {
            wire_session_id: root.expect("checked").to_owned(),
            child_session_id: Some(actual_session_id.to_owned()),
            grant_session_id: root.expect("checked").to_owned(),
        }
    }
}

/// One request's client-visible and durable coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedSession {
    wire_session_id: String,
    child_session_id: Option<String>,
    grant_session_id: String,
}

impl RoutedSession {
    #[must_use]
    pub fn direct(session_id: &str) -> Self {
        Self {
            wire_session_id: session_id.to_owned(),
            child_session_id: None,
            grant_session_id: session_id.to_owned(),
        }
    }

    #[must_use]
    pub fn wire_session_id(&self) -> &str {
        &self.wire_session_id
    }

    #[must_use]
    pub fn child_session_id(&self) -> Option<&str> {
        self.child_session_id.as_deref()
    }

    #[must_use]
    pub fn grant_session_id(&self) -> &str {
        &self.grant_session_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_routes_children_to_the_declared_root() {
        let route = AcpSessionRoute::new(false);
        route.bind_root("ses-root").expect("bind root");

        assert_eq!(
            route.resolve("ses-child"),
            RoutedSession {
                wire_session_id: "ses-root".to_owned(),
                child_session_id: Some("ses-child".to_owned()),
                grant_session_id: "ses-root".to_owned(),
            }
        );
    }

    #[test]
    fn negotiated_subagents_keep_the_child_wire_identity() {
        let route = AcpSessionRoute::new(true);
        route.bind_root("ses-root").expect("bind root");

        assert_eq!(
            route.resolve("ses-child"),
            RoutedSession {
                wire_session_id: "ses-child".to_owned(),
                child_session_id: None,
                grant_session_id: "ses-root".to_owned(),
            }
        );
    }
}

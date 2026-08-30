//! Interface-neutral commands that mutate one durable session without entering the model loop.

/// A native session control command shared by client surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommand {
    /// Summarize older durable history and keep the recent tail verbatim.
    Compact,
    /// Inspect or mutate the durable top-level goal for this session.
    Goal,
    /// Enter Plan mode, or resume Work mode when already planning.
    Plan,
    /// Enter the read-only planning Agent immediately.
    StartPlan,
    /// Resume implementation from the durable plan.
    StartWork,
}

impl SessionCommand {
    /// Every native session command clients may advertise.
    pub const ALL: [Self; 5] = [
        Self::Compact,
        Self::Goal,
        Self::Plan,
        Self::StartPlan,
        Self::StartWork,
    ];

    /// Stable slash/wire name without the leading `/`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Goal => "goal",
            Self::Plan => "plan",
            Self::StartPlan => "start-plan",
            Self::StartWork => "start-work",
        }
    }

    /// Human-readable discovery description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Compact => "Summarize older context and keep the recent turn tail",
            Self::Goal => "Set, view, or manage the durable session goal",
            Self::Plan => "Enter Plan mode, or resume Work mode when already planning",
            Self::StartPlan => "Enter read-only Plan mode immediately",
            Self::StartWork => "Resume implementation from the durable plan",
        }
    }

    /// Whether the command owns an unparsed argument tail.
    #[must_use]
    pub const fn accepts_arguments(self) -> bool {
        matches!(self, Self::Goal)
    }

    /// Optional completion hint for clients that render command arguments.
    #[must_use]
    pub const fn input_hint(self) -> Option<&'static str> {
        match self {
            Self::Goal => Some("objective | action [value]"),
            Self::Compact | Self::Plan | Self::StartPlan | Self::StartWork => None,
        }
    }

    /// Whether the command replaces the active collaboration-mode host.
    #[must_use]
    pub const fn is_mode_control(self) -> bool {
        matches!(self, Self::Plan | Self::StartPlan | Self::StartWork)
    }

    /// Resolve one exact native command name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::SessionCommand;

    #[test]
    fn native_session_commands_have_stable_unique_names() {
        let names = SessionCommand::ALL
            .into_iter()
            .map(SessionCommand::name)
            .collect::<Vec<_>>();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();

        assert_eq!(names, unique);
        assert_eq!(
            names,
            ["compact", "goal", "plan", "start-plan", "start-work"]
        );
        assert_eq!(
            SessionCommand::from_name("compact"),
            Some(SessionCommand::Compact)
        );
        assert_eq!(
            SessionCommand::from_name("goal"),
            Some(SessionCommand::Goal)
        );
        assert_eq!(
            SessionCommand::from_name("plan"),
            Some(SessionCommand::Plan)
        );
        assert_eq!(
            SessionCommand::from_name("start-plan"),
            Some(SessionCommand::StartPlan)
        );
        assert_eq!(
            SessionCommand::from_name("start-work"),
            Some(SessionCommand::StartWork)
        );
        assert_eq!(SessionCommand::from_name("review"), None);
    }

    #[test]
    fn only_goal_accepts_a_free_form_argument_tail() {
        assert!(SessionCommand::Goal.accepts_arguments());
        assert_eq!(
            SessionCommand::Goal.input_hint(),
            Some("objective | action [value]")
        );
        for command in [
            SessionCommand::Compact,
            SessionCommand::Plan,
            SessionCommand::StartPlan,
            SessionCommand::StartWork,
        ] {
            assert!(!command.accepts_arguments(), "{command:?}");
            assert_eq!(command.input_hint(), None, "{command:?}");
        }
    }
}

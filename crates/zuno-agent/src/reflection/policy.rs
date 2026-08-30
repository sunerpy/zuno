use std::collections::{HashMap, HashSet};

/// Safety exclusions carried verbatim from Hermes background review.
pub const NEGATIVE_LEARNING_LIST: [&str; 5] = [
    "Environment-dependent failures: missing binaries, fresh-install errors, post-migration path mismatches, 'command not found', unconfigured credentials, uninstalled packages. The user can fix these — they are not durable rules.",
    "Negative claims about tools or features ('browser tools do not work', 'X tool is broken', 'cannot use Y from execute_code'). These harden into refusals the agent cites against itself for months after the actual problem was fixed.",
    "Session-specific transient errors that resolved before the conversation ended. If retrying worked, the lesson is the retry pattern, not the original failure.",
    "One-off task narratives. A user asking 'summarize today's market' or 'analyze this PR' is not a class of work that warrants a skill.",
    "Unresolved failures: if the session ended WITHOUT actually finding a working method — you tried several things, none worked, and told the user to check manually — do NOT write those attempts up as a 'reliable workflow' or 'recommended approach'. That presents an untested sequence of failures as validated guidance a future session will trust and repeat. Either say 'Nothing to save', or, only if you are independently confident of a real working alternative (not something you are merely guessing might work), capture ONLY that alternative — never the dead ends, and never dressed up as best practice.",
];

/// Terminal result of a command represented in the replay transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The command completed successfully.
    Succeeded { output: String },
    /// The command completed unsuccessfully.
    Failed { output: String },
}

impl CommandOutcome {
    /// Construct a successful command result.
    #[must_use]
    pub fn succeeded(output: impl Into<String>) -> Self {
        Self::Succeeded {
            output: output.into(),
        }
    }

    /// Construct a failed command result.
    #[must_use]
    pub fn failed(output: impl Into<String>) -> Self {
        Self::Failed {
            output: output.into(),
        }
    }
}

/// A model-relevant event from the completed foreground turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    /// User-authored text.
    User { text: String },
    /// Assistant-authored text.
    Assistant { text: String },
    /// One terminal command attempt.
    Command {
        command: String,
        outcome: CommandOutcome,
    },
}

impl TranscriptEvent {
    /// Construct a user transcript event.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User { text: text.into() }
    }

    /// Construct an assistant transcript event.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant { text: text.into() }
    }

    /// Construct a command transcript event.
    #[must_use]
    pub fn command(command: impl Into<String>, outcome: CommandOutcome) -> Self {
        Self::Command {
            command: command.into(),
            outcome,
        }
    }
}

/// An owned replay of one foreground turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnTranscript {
    events: Vec<TranscriptEvent>,
}

/// Durable scheduling facts derived from one completed transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionEligibility {
    pub recovered: bool,
    pub negative_learning: bool,
}

impl TurnTranscript {
    /// Construct a transcript in chronological order.
    #[must_use]
    pub fn new(events: Vec<TranscriptEvent>) -> Self {
        Self { events }
    }

    /// Events in chronological order for a provider-facing reflection runner.
    #[must_use]
    pub fn events(&self) -> &[TranscriptEvent] {
        &self.events
    }

    /// Classify one transcript before the SQLite scheduler advances its cadence.
    #[must_use]
    pub fn reflection_eligibility(&self) -> ReflectionEligibility {
        ReflectionEligibility {
            recovered: self.has_failure_recovery(),
            negative_learning: self.is_negative_learning(),
        }
    }

    fn has_failure_recovery(&self) -> bool {
        let mut failed = HashSet::new();
        for event in &self.events {
            let TranscriptEvent::Command { command, outcome } = event else {
                continue;
            };
            match outcome {
                CommandOutcome::Failed { .. } => {
                    failed.insert(command.as_str());
                }
                CommandOutcome::Succeeded { .. } if failed.contains(command.as_str()) => {
                    return true;
                }
                CommandOutcome::Succeeded { .. } => {}
            }
        }
        false
    }

    fn is_negative_learning(&self) -> bool {
        self.has_environment_failure()
            || self.has_negative_tool_claim()
            || self.has_transient_retry()
            || self.has_one_off_narrative()
            || self.has_unresolved_failure()
    }

    fn has_environment_failure(&self) -> bool {
        const MARKERS: [&str; 8] = [
            "command not found",
            "no such file or directory",
            "missing binary",
            "unconfigured credential",
            "credentials not configured",
            "not installed",
            "package is not installed",
            "fresh install",
        ];
        self.events.iter().any(|event| {
            let TranscriptEvent::Command {
                outcome: CommandOutcome::Failed { output },
                ..
            } = event
            else {
                return false;
            };
            let output = output.to_ascii_lowercase();
            MARKERS.iter().any(|marker| output.contains(marker))
        })
    }

    fn has_negative_tool_claim(&self) -> bool {
        self.events.iter().any(|event| {
            let text = match event {
                TranscriptEvent::User { text } | TranscriptEvent::Assistant { text } => text,
                TranscriptEvent::Command { .. } => return false,
            };
            let text = text.to_ascii_lowercase();
            (text.contains("tool") || text.contains("feature"))
                && (text.contains("is broken")
                    || text.contains("does not work")
                    || text.contains("do not work")
                    || (text.contains("cannot use") && text.contains("execute_code")))
        })
    }

    fn has_transient_retry(&self) -> bool {
        self.events.windows(2).any(|events| {
            matches!(
                events,
                [
                    TranscriptEvent::Command {
                        command: failed_command,
                        outcome: CommandOutcome::Failed { .. },
                    },
                    TranscriptEvent::Command {
                        command: succeeded_command,
                        outcome: CommandOutcome::Succeeded { .. },
                    },
                ] if failed_command == succeeded_command
            )
        })
    }

    fn has_one_off_narrative(&self) -> bool {
        self.events.iter().any(|event| {
            let TranscriptEvent::User { text } = event else {
                return false;
            };
            let text = text.to_ascii_lowercase();
            text.contains("summarize today's")
                || text.contains("summarise today's")
                || text.contains("analyze this pr")
                || text.contains("analyse this pr")
        })
    }

    fn has_unresolved_failure(&self) -> bool {
        let mut commands = HashMap::new();
        for event in &self.events {
            let TranscriptEvent::Command { command, outcome } = event else {
                continue;
            };
            commands.insert(
                command.as_str(),
                matches!(outcome, CommandOutcome::Succeeded { .. }),
            );
        }
        commands.values().any(|succeeded| !succeeded)
    }
}

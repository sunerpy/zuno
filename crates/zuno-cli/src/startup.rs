//! Startup phase timing, for attributing a startup-budget failure.
//!
//! A budget that only reports a total tells you the process got slower and
//! nothing about where. §8.2 of the perf plan makes the phase marks a companion
//! requirement to the budget for that reason: an over-budget run has to name the
//! segment that grew.
//!
//! # Cost when the profile is off
//!
//! One [`Instant::now`] per mark and nothing else — no allocation, no formatting,
//! no environment read per mark. The environment is read once, at
//! [`StartupProfile::new`]. Marks are recorded unconditionally because a
//! conditional mark would make the profiled and unprofiled paths different code.
//!
//! # No waits
//!
//! Nothing here waits on anything. The profile is written to stderr with a single
//! `write_all` at [`StartupProfile::emit`], which is why it cannot become the
//! startup step that hangs.

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Instant;

/// Set to any non-empty value to have the profile written to stderr at exit.
pub const ZUNO_STARTUP_PROFILE: &str = "ZUNO_STARTUP_PROFILE";

/// Prefix every profile line carries, so a consumer can find them in stderr.
pub const PROFILE_LINE_PREFIX: &str = "zuno-startup";

/// The ordered startup segments a profile can report.
///
/// Exhaustive and ordered: the budget test derives its expected phase set from
/// [`StartupPhase::ALL`], so a new segment has to be named here before it can be
/// marked, and a removed one fails the test rather than silently disappearing
/// from the attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StartupPhase {
    /// Allocator tuning check and child-process guard activation.
    ProcessGuard,
    /// `clap` argument parsing.
    Parse,
    /// Resolving the process environment into a startup environment.
    Environment,
    /// The Unix `exec` that hands the command process its environment.
    ///
    /// Present only on Unix, and there only on invocations that dispatch a
    /// command. `--version` and a `clap` error return before it, which is why they
    /// are the fast paths. Platforms without `exec` never reach this phase: they
    /// apply the resolved environment in this process rather than starting a second
    /// one, so a dispatching invocation there writes one profile line, not two.
    BootstrapRestart,
    /// Building the tracing subscriber and opening the structured log store.
    Logging,
    /// Everything from the last mark to process exit.
    Dispatch,
}

impl StartupPhase {
    /// Every phase, in the order startup passes through them.
    pub const ALL: [Self; 6] = [
        Self::ProcessGuard,
        Self::Parse,
        Self::Environment,
        Self::BootstrapRestart,
        Self::Logging,
        Self::Dispatch,
    ];

    /// The stable name used in profile lines.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessGuard => "process_guard",
            Self::Parse => "parse",
            Self::Environment => "environment",
            Self::BootstrapRestart => "bootstrap_restart",
            Self::Logging => "logging",
            Self::Dispatch => "dispatch",
        }
    }
}

/// Accumulated marks for one process.
#[derive(Debug)]
pub struct StartupProfile {
    started: Instant,
    last: Instant,
    marks: Vec<(StartupPhase, u128)>,
    enabled: bool,
}

impl StartupProfile {
    /// Begin a profile, reading the environment exactly once.
    #[must_use]
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            marks: Vec::with_capacity(StartupPhase::ALL.len()),
            enabled: std::env::var_os(ZUNO_STARTUP_PROFILE).is_some_and(|value| !value.is_empty()),
        }
    }

    /// Close `phase` at the current instant.
    pub fn mark(&mut self, phase: StartupPhase) {
        let now = Instant::now();
        self.marks
            .push((phase, now.duration_since(self.last).as_micros()));
        self.last = now;
    }

    /// Microseconds since the profile began.
    #[must_use]
    pub fn total_micros(&self) -> u128 {
        self.started.elapsed().as_micros()
    }

    /// Write the profile to stderr, or do nothing when it was not requested.
    ///
    /// stderr, never stdout: `zuno --version` and the stdio protocols put parseable
    /// bytes on stdout, and `zuno-observability`'s crate docs record why a stray
    /// byte there is a protocol parse error rather than noise.
    pub fn emit(&mut self, phase: StartupPhase) {
        self.mark(phase);
        if !self.enabled {
            return;
        }
        let mut line = String::with_capacity(128);
        let _ = write!(
            line,
            "{PROFILE_LINE_PREFIX} total_us={}",
            self.total_micros()
        );
        for (phase, micros) in &self.marks {
            let _ = write!(line, " {}_us={micros}", phase.as_str());
        }
        line.push('\n');
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

impl Default for StartupProfile {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phase_has_a_distinct_snake_case_name() {
        let mut seen = std::collections::BTreeSet::new();
        for phase in StartupPhase::ALL {
            let name = phase.as_str();
            assert!(seen.insert(name), "duplicate phase name {name}");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is not snake_case, so the `<phase>_us=` key would not parse"
            );
        }
        assert_eq!(seen.len(), StartupPhase::ALL.len());
    }

    #[test]
    fn a_profile_that_was_not_requested_records_marks_but_writes_nothing() {
        // Given: a profile with the emit path disabled, which is the shipped path.
        let mut profile = StartupProfile {
            started: Instant::now(),
            last: Instant::now(),
            marks: Vec::new(),
            enabled: false,
        };

        // When: the same marks a real startup takes are recorded.
        profile.mark(StartupPhase::ProcessGuard);
        profile.emit(StartupPhase::Dispatch);

        // Then: both marks exist, so profiled and unprofiled startup run the same
        // code and the profile cannot describe a path users do not take.
        assert_eq!(profile.marks.len(), 2);
        assert_eq!(profile.marks[0].0, StartupPhase::ProcessGuard);
        assert_eq!(profile.marks[1].0, StartupPhase::Dispatch);
    }
}

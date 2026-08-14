//! The two experimental flags that decide whether anything is watched at all.
//!
//! # Why these two are not `Flag.truthy`
//!
//! Every other flag in `flag/flag.ts` goes through `Flag.truthy` (`flag.ts:3-6`),
//! which accepts only a lower-cased `"true"` or `"1"`. These two do not — they
//! are declared with Effect's `Config.boolean` (`flag/flag.ts:37-42`):
//!
//! ```ts
//! OPENCODE_EXPERIMENTAL_FILEWATCHER: Config.boolean("OPENCODE_EXPERIMENTAL_FILEWATCHER").pipe(
//!   Config.withDefault(false),
//! ),
//! ```
//!
//! `Config.boolean` accepts a wider set than `truthy` and, crucially, **fails**
//! on a value it cannot parse instead of quietly reading it as `false`. That
//! difference is observable, so it is modelled here rather than flattened into
//! [`oc_paths::Env::flag`]. See `.omo/notepads/opencode-rust/issues.md` for the
//! part of this that could not be confirmed against the 1.18.12 binary.
//!
//! # Why an unparseable value disables the watcher
//!
//! `Config.withDefault(false)` supplies `false` only when the variable is
//! *absent*. A present-but-unparseable value yields an Effect `InvalidData`
//! failure, which propagates out of the `Layer.effect` body into the
//! `Effect.catchCause` wrapping it (`filesystem/watcher.ts:130-136`); that
//! handler logs and returns an empty service. So a typo in either variable does
//! not fall back to "watcher off but still watching `.git`" — it takes the whole
//! layer down, including the `.git` subscription. [`Decision::Disabled`] is that
//! outcome, and it carries the reason so the caller can log what the oracle logs.

use oc_paths::Env;

/// `OPENCODE_EXPERIMENTAL_FILEWATCHER` (`flag/flag.ts:37-39`).
pub const OPENCODE_EXPERIMENTAL_FILEWATCHER: &str = "OPENCODE_EXPERIMENTAL_FILEWATCHER";

/// `OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER` (`flag/flag.ts:40-42`).
pub const OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER: &str =
    "OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER";

/// The values Effect's `Config.boolean` reads as true, lower-cased.
const TRUE_VALUES: [&str; 4] = ["true", "1", "yes", "on"];

/// The values Effect's `Config.boolean` reads as false, lower-cased.
const FALSE_VALUES: [&str; 4] = ["false", "0", "no", "off"];

/// Why the watcher declined to start.
///
/// Kept as data rather than folded into a bare `bool` because the two reasons
/// are logged differently by the oracle — a disable flag is a deliberate opt-out
/// and logs nothing, a bad value is a misconfiguration the user wants told about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisabledReason {
    /// `OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER` was set to a true value.
    ///
    /// Checked first in `watcher.ts:59`, before the backend or the binding, so
    /// it wins over every other input including the enable flag.
    ExplicitlyDisabled,
    /// One of the two variables held a value `Config.boolean` cannot parse.
    ///
    /// Carries the variable name and the offending value so the caller can log
    /// the same detail the oracle's `Cause.pretty` would.
    UnparseableFlag {
        /// The variable whose value failed to parse.
        key: &'static str,
        /// The value as it was read, untouched.
        value: String,
    },
}

/// What the flags say to do.
///
/// The two enabled variants are separate because the oracle watches two
/// different things under two different conditions (`watcher.ts:107-125`) and a
/// caller has to be able to tell them apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Watch nothing. The whole layer is a no-op.
    Disabled(DisabledReason),
    /// Watch only the VCS metadata directory.
    ///
    /// This is the default: `OPENCODE_EXPERIMENTAL_FILEWATCHER` gates *only* the
    /// project-directory subscription (`watcher.ts:107`), while the `.git`
    /// subscription at `watcher.ts:112` is gated on nothing but the repository
    /// being git. With no flags set at all, the oracle therefore still watches
    /// `.git`, and a port that treats the enable flag as a master switch would
    /// silently stop noticing branch switches.
    VcsOnly,
    /// Watch the project directory as well as the VCS metadata directory.
    Full,
}

impl Decision {
    /// Whether the project directory should be watched.
    #[must_use]
    pub fn watches_project(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether the VCS metadata directory should be watched.
    #[must_use]
    pub fn watches_vcs(&self) -> bool {
        matches!(self, Self::Full | Self::VcsOnly)
    }

    /// Whether anything at all should be watched.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled(_))
    }
}

/// Parse one variable with Effect `Config.boolean` semantics.
///
/// `Ok(None)` means absent, which `Config.withDefault(false)` turns into
/// `false`; the distinction is kept here because only the caller knows what the
/// default for a given key is.
fn parse(env: &Env, key: &'static str) -> Result<Option<bool>, DisabledReason> {
    let Some(raw) = env.value(key) else {
        return Ok(None);
    };
    let lowered = raw.to_ascii_lowercase();
    if TRUE_VALUES.contains(&lowered.as_str()) {
        return Ok(Some(true));
    }
    if FALSE_VALUES.contains(&lowered.as_str()) {
        return Ok(Some(false));
    }
    Err(DisabledReason::UnparseableFlag {
        key,
        value: raw.to_owned(),
    })
}

/// Resolve both flags into a single decision.
///
/// Order matters and mirrors `watcher.ts` exactly: disable is read first and
/// short-circuits, so setting both variables to true leaves the watcher off.
#[must_use]
pub fn decide(env: &Env) -> Decision {
    let disable = match parse(env, OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER) {
        Ok(value) => value.unwrap_or(false),
        Err(reason) => return Decision::Disabled(reason),
    };
    if disable {
        return Decision::Disabled(DisabledReason::ExplicitlyDisabled);
    }
    match parse(env, OPENCODE_EXPERIMENTAL_FILEWATCHER) {
        Ok(value) if value.unwrap_or(false) => Decision::Full,
        Ok(_) => Decision::VcsOnly,
        Err(reason) => Decision::Disabled(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_flags_watch_only_the_vcs_directory() {
        assert_eq!(decide(&Env::empty()), Decision::VcsOnly);
    }

    #[test]
    fn enable_flag_adds_the_project_directory() {
        let env = Env::empty().with("ZUNO_EXPERIMENTAL_FILEWATCHER", "true");
        assert_eq!(decide(&env), Decision::Full);
        assert!(decide(&env).watches_project());
        assert!(decide(&env).watches_vcs());
    }

    #[test]
    fn disable_wins_when_both_flags_are_set() {
        let env = Env::empty()
            .with("ZUNO_EXPERIMENTAL_FILEWATCHER", "true")
            .with("ZUNO_EXPERIMENTAL_DISABLE_FILEWATCHER", "true");
        assert_eq!(
            decide(&env),
            Decision::Disabled(DisabledReason::ExplicitlyDisabled)
        );
    }

    #[test]
    fn effect_boolean_accepts_more_than_flag_truthy() {
        for value in ["true", "TRUE", "1", "yes", "YES", "on", "On"] {
            let env = Env::empty().with("ZUNO_EXPERIMENTAL_FILEWATCHER", value);
            assert_eq!(decide(&env), Decision::Full, "{value} should enable");
        }
        for value in ["false", "FALSE", "0", "no", "off"] {
            let env = Env::empty().with("ZUNO_EXPERIMENTAL_FILEWATCHER", value);
            assert_eq!(decide(&env), Decision::VcsOnly, "{value} should not enable");
        }
    }

    #[test]
    fn an_unparseable_value_disables_everything() {
        let env = Env::empty().with("ZUNO_EXPERIMENTAL_FILEWATCHER", "bogus");
        assert_eq!(
            decide(&env),
            Decision::Disabled(DisabledReason::UnparseableFlag {
                key: OPENCODE_EXPERIMENTAL_FILEWATCHER,
                value: "bogus".to_owned(),
            })
        );
        // Including the `.git` subscription, which is otherwise ungated.
        assert!(!decide(&env).watches_vcs());
    }

    #[test]
    fn an_unparseable_disable_value_also_disables_everything() {
        let env = Env::empty().with("ZUNO_EXPERIMENTAL_DISABLE_FILEWATCHER", "maybe");
        assert_eq!(
            decide(&env),
            Decision::Disabled(DisabledReason::UnparseableFlag {
                key: OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER,
                value: "maybe".to_owned(),
            })
        );
    }

    #[test]
    fn an_empty_value_is_unparseable_not_absent() {
        // `Config.boolean` reads the variable as present with the empty string,
        // which is in neither value set. This is where it diverges from
        // `Env::truthy_value`'s JavaScript `||` rule.
        let env = Env::empty().with("ZUNO_EXPERIMENTAL_DISABLE_FILEWATCHER", "");
        assert!(decide(&env).is_disabled());
    }
}

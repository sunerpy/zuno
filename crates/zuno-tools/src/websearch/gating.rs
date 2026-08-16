//! Which search provider is used, and whether the search tool is offered at all.
//!
//! # Two independent questions
//!
//! Upstream answers them in two different functions, and conflating them is the bug
//! this module exists to avoid:
//!
//! - **Is `websearch` offered?** [`web_search_enabled`], from
//!   `packages/opencode/src/tool/registry.ts:58-60`. Reads the *model provider* and
//!   the two enable flags. This is a visibility question, evaluated before the tool
//!   list is built.
//! - **Which backend runs the query?** [`select_provider`], from
//!   `packages/opencode/src/tool/websearch.ts:33-41`. Reads an env override, the
//!   flags, then falls back to a per-session coin flip. This is a routing question,
//!   evaluated at call time.
//!
//! `OPENCODE_WEBSEARCH_PROVIDER` answers only the second. Setting it does **not**
//! make the tool appear: `webSearchEnabled` never reads it. A verified surprise, and
//! the reason the two are separate functions here as well.
//!
//! # Why absence rather than a failing call
//!
//! A tool the model can never use successfully still costs its schema in prompt
//! tokens on every request, and invites a spiral where the model calls it, is
//! refused, and then reasons about the refusal. So an unconfigured `websearch` is
//! absent from the list. [`WebError::NoSearchProvider`] exists only so that a
//! registry bug which exposes it anyway fails by name instead of confusingly.

use crate::webfetch::bounds::WebError;

/// The model provider whose sessions get search without any flag.
///
/// Oracle: `providerID === ProviderV2.ID.opencode`
/// (`packages/opencode/src/tool/registry.ts:59`).
pub const HOSTED_PROVIDER_ID: &str = "opencode";

/// Selects the backend, overriding the flags and the session coin flip.
///
/// Oracle: `packages/opencode/src/tool/websearch.ts:34`. Only `exa` and `parallel`
/// are honoured; any other value is ignored rather than being an error.
pub const ENV_PROVIDER: &str = "OPENCODE_WEBSEARCH_PROVIDER";

/// Enables Exa, and with it the tool.
pub const ENV_ENABLE_EXA: &str = "OPENCODE_ENABLE_EXA";

/// The pre-rename spelling of [`ENV_ENABLE_EXA`], still honoured.
pub const ENV_LEGACY_EXA: &str = "OPENCODE_EXPERIMENTAL_EXA";

/// Enables Parallel, and with it the tool.
pub const ENV_ENABLE_PARALLEL: &str = "OPENCODE_ENABLE_PARALLEL";

/// The pre-rename spelling of [`ENV_ENABLE_PARALLEL`], still honoured.
pub const ENV_LEGACY_PARALLEL: &str = "OPENCODE_EXPERIMENTAL_PARALLEL";

/// The blanket experimental switch.
///
/// **Enables Exa only.** `packages/opencode/src/effect/runtime-flags.ts:31-39`
/// includes `experimental` in `enableExa`'s disjunction and omits it from
/// `enableParallel`'s; `packages/core/src/tool/websearch.ts:79-80` repeats the same
/// asymmetry. Verified in both, so it is deliberate rather than a typo in one place.
pub const ENV_EXPERIMENTAL: &str = "OPENCODE_EXPERIMENTAL";

/// Exa's API key, appended to the MCP URL as the `exaApiKey` query parameter.
///
/// Oracle: `packages/core/src/tool/websearch.ts:81,145-150`.
pub const ENV_EXA_API_KEY: &str = "EXA_API_KEY";

/// Parallel's API key, sent as an `Authorization: Bearer` header.
///
/// Oracle: `packages/opencode/src/tool/websearch.ts:58-62`.
pub const ENV_PARALLEL_API_KEY: &str = "PARALLEL_API_KEY";

/// The search backend a query is routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Exa, over its hosted MCP endpoint.
    Exa,
    /// Parallel, over its hosted MCP endpoint.
    Parallel,
}

impl Provider {
    /// The wire name, as it appears in metadata and in the env override.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exa => "exa",
            Self::Parallel => "parallel",
        }
    }

    /// The label shown in the transcript.
    ///
    /// Oracle: `webSearchProviderLabel` (`packages/opencode/src/tool/websearch.ts:43-47`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Exa => "Exa Web Search",
            Self::Parallel => "Parallel Web Search",
        }
    }

    /// Parses the env override, ignoring anything that is not a known provider.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exa" => Some(Self::Exa),
            "parallel" => Some(Self::Parallel),
            _ => None,
        }
    }
}

/// Everything the search tool needs from the environment, resolved once.
///
/// Held as data rather than read from `std::env` at call time so a test can state a
/// configuration without mutating process globals, and so todo 44 can source a key
/// from somewhere other than the environment without editing this tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchConfig {
    /// The forced backend, from [`ENV_PROVIDER`].
    pub provider: Option<Provider>,
    /// Whether Exa is enabled, which also makes the tool visible.
    pub enable_exa: bool,
    /// Whether Parallel is enabled, which also makes the tool visible.
    pub enable_parallel: bool,
    /// Exa's API key, if one is configured.
    pub exa_api_key: Option<String>,
    /// Parallel's API key, if one is configured.
    pub parallel_api_key: Option<String>,
}

impl SearchConfig {
    /// Reads the configuration from the process environment.
    ///
    /// # Where the keys come from
    ///
    /// The environment, and only the environment. Upstream reads
    /// `process.env.EXA_API_KEY` and `process.env.PARALLEL_API_KEY` directly
    /// (`packages/core/src/tool/websearch.ts:81-82`) and never consults the
    /// credential store — `auth.json` holds *model provider* credentials, and neither
    /// `exa` nor `parallel` is a model provider there. Checked rather than assumed.
    /// A caller that wants a key from elsewhere constructs this struct itself.
    #[must_use]
    pub fn from_env() -> Self {
        let env = zuno_paths::Env::from_process();
        Self::from_lookup(|key| env.value(key).map(str::to_owned))
    }

    /// Reads the configuration through a caller-supplied lookup, for tests.
    #[must_use]
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let truthy = |key: &str| lookup(key).is_some_and(|value| is_truthy(&value));
        let experimental = truthy(ENV_EXPERIMENTAL);

        Self {
            provider: lookup(ENV_PROVIDER).as_deref().and_then(Provider::parse),
            enable_exa: experimental || truthy(ENV_ENABLE_EXA) || truthy(ENV_LEGACY_EXA),
            enable_parallel: truthy(ENV_ENABLE_PARALLEL) || truthy(ENV_LEGACY_PARALLEL),
            exa_api_key: lookup(ENV_EXA_API_KEY).filter(|key| !key.is_empty()),
            parallel_api_key: lookup(ENV_PARALLEL_API_KEY).filter(|key| !key.is_empty()),
        }
    }

    /// The API key for `provider`, if configured.
    #[must_use]
    pub fn api_key(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::Exa => self.exa_api_key.as_deref(),
            Provider::Parallel => self.parallel_api_key.as_deref(),
        }
    }
}

/// Whether an env value counts as set.
///
/// Effect's `Config.boolean` accepts the usual spellings; an unset or empty variable
/// is false. `"0"` and `"false"` are explicitly false so that `EXA=0` disables rather
/// than enabling by mere presence.
fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Whether `websearch` is offered to the model at all.
///
/// Oracle, verbatim in structure
/// (`packages/opencode/src/tool/registry.ts:58-60`):
///
/// ```text
/// providerID === ProviderV2.ID.opencode || flags.exa || flags.parallel
/// ```
///
/// `provider_id` is the **model** provider serving the turn, not the search backend.
/// A hosted-`opencode` session gets search with no flags; every other model provider
/// needs one of the two enable flags.
#[must_use]
pub fn web_search_enabled(provider_id: &str, config: &SearchConfig) -> bool {
    provider_id == HOSTED_PROVIDER_ID || config.enable_exa || config.enable_parallel
}

/// Picks the backend for one session.
///
/// Oracle: `packages/opencode/src/tool/websearch.ts:33-41` — env override, then the
/// parallel flag, then the exa flag, then a deterministic per-session split so a
/// session keeps one backend for its whole life instead of alternating.
#[must_use]
pub fn select_provider(session_id: &str, config: &SearchConfig) -> Provider {
    if let Some(forced) = config.provider {
        return forced;
    }
    if config.enable_parallel {
        return Provider::Parallel;
    }
    if config.enable_exa {
        return Provider::Exa;
    }
    if session_hash(session_id).is_multiple_of(2) {
        Provider::Exa
    } else {
        Provider::Parallel
    }
}

/// The session's [`checksum`] read back as a number, `0` when there is no session id.
///
/// Oracle: `Number.parseInt(checksum(sessionID) ?? "0", 36)`.
fn session_hash(session_id: &str) -> u64 {
    checksum(session_id)
        .and_then(|digits| u64::from_str_radix(&digits, 36).ok())
        .unwrap_or(0)
}

/// FNV-1a over the string's UTF-16 code units, rendered base 36.
///
/// Oracle: `packages/core/src/util/encode.ts:22-30`. Two details that a
/// reimplementation gets wrong by default:
///
/// - **UTF-16 code units, not bytes.** `charCodeAt` yields code units, so a
///   non-ASCII session id hashes differently than its UTF-8 bytes would.
/// - **Wrapping 32-bit multiply.** `Math.imul` is a deliberate int32 multiply;
///   promoting to 64 bits changes every subsequent digit.
///
/// Returns `None` for the empty string, as the oracle does, because the caller
/// substitutes `"0"` for that case and the distinction is observable.
#[must_use]
pub fn checksum(content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }

    let mut hash: u32 = 0x811c_9dc5;
    for unit in content.encode_utf16() {
        hash ^= u32::from(unit);
        hash = hash.wrapping_mul(0x0100_0193);
    }

    Some(base36(hash))
}

/// Renders a `u32` the way JavaScript's `Number.prototype.toString(36)` does.
fn base36(mut value: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }

    let mut digits = Vec::new();
    while value > 0 {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base36 digits are ASCII")
}

/// The failure a caller raises when it has no provider to route to.
#[must_use]
pub fn no_provider() -> WebError {
    WebError::NoSearchProvider
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> SearchConfig {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        SearchConfig::from_lookup(|key| {
            owned
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        })
    }

    // --- checksum parity: values captured from the oracle's own implementation ---

    #[test]
    fn the_checksum_matches_the_oracle_digit_for_digit() {
        // Captured by running `packages/core/src/util/encode.ts`'s `checksum`.
        assert_eq!(checksum("ses_test").as_deref(), Some("1raoqcz"));
        assert_eq!(checksum("ses_abc123").as_deref(), Some("t3lfaf"));
        assert_eq!(checksum("a").as_deref(), Some("1r9wi7g"));
        assert_eq!(checksum("session-42").as_deref(), Some("1c39bh0"));
    }

    #[test]
    fn the_checksum_hashes_utf16_code_units_not_utf8_bytes() {
        // "会话" is 6 UTF-8 bytes but 2 UTF-16 code units; the oracle yields e8q640.
        assert_eq!(checksum("会话").as_deref(), Some("e8q640"));
    }

    #[test]
    fn an_empty_session_id_has_no_checksum() {
        assert_eq!(checksum(""), None);
    }

    #[test]
    fn the_session_split_matches_the_oracle_for_captured_ids() {
        let unflagged = SearchConfig::default();
        assert_eq!(select_provider("ses_test", &unflagged), Provider::Parallel);
        assert_eq!(
            select_provider("ses_abc123", &unflagged),
            Provider::Parallel
        );
        assert_eq!(select_provider("a", &unflagged), Provider::Exa);
        assert_eq!(select_provider("session-42", &unflagged), Provider::Exa);
        assert_eq!(select_provider("会话", &unflagged), Provider::Exa);
    }

    #[test]
    fn an_empty_session_id_falls_to_exa() {
        assert_eq!(select_provider("", &SearchConfig::default()), Provider::Exa);
    }

    #[test]
    fn the_session_split_is_stable_across_calls() {
        let unflagged = SearchConfig::default();
        let first = select_provider("ses_stability", &unflagged);
        for _ in 0..16 {
            assert_eq!(select_provider("ses_stability", &unflagged), first);
        }
    }

    // --- visibility ---

    #[test]
    fn the_hosted_provider_gets_search_with_no_flags() {
        assert!(web_search_enabled("opencode", &SearchConfig::default()));
    }

    #[test]
    fn another_model_provider_with_no_flags_does_not_get_search() {
        assert!(!web_search_enabled("openai", &SearchConfig::default()));
        assert!(!web_search_enabled("anthropic", &SearchConfig::default()));
    }

    #[test]
    fn either_enable_flag_makes_search_visible() {
        assert!(web_search_enabled(
            "openai",
            &config(&[(ENV_ENABLE_EXA, "true")])
        ));
        assert!(web_search_enabled(
            "openai",
            &config(&[(ENV_ENABLE_PARALLEL, "true")])
        ));
    }

    #[test]
    fn the_legacy_flag_spellings_still_work() {
        assert!(web_search_enabled(
            "openai",
            &config(&[(ENV_LEGACY_EXA, "1")])
        ));
        assert!(web_search_enabled(
            "openai",
            &config(&[(ENV_LEGACY_PARALLEL, "1")])
        ));
    }

    #[test]
    fn the_blanket_experimental_flag_enables_exa_only() {
        // The asymmetry is upstream's, verified in runtime-flags.ts and core.
        let flags = config(&[(ENV_EXPERIMENTAL, "true")]);
        assert!(flags.enable_exa);
        assert!(!flags.enable_parallel);
        assert!(web_search_enabled("openai", &flags));
    }

    #[test]
    fn the_provider_override_alone_does_not_make_search_visible() {
        // The surprise worth pinning: OPENCODE_WEBSEARCH_PROVIDER routes, it does not
        // enable. `webSearchEnabled` never reads it.
        let flags = config(&[(ENV_PROVIDER, "exa")]);
        assert_eq!(flags.provider, Some(Provider::Exa));
        assert!(!web_search_enabled("openai", &flags));
    }

    #[test]
    fn a_falsy_flag_value_does_not_enable_by_mere_presence() {
        assert!(!web_search_enabled(
            "openai",
            &config(&[(ENV_ENABLE_EXA, "0"), (ENV_ENABLE_PARALLEL, "false")])
        ));
    }

    // --- routing precedence ---

    #[test]
    fn the_override_beats_both_flags() {
        let flags = config(&[(ENV_PROVIDER, "exa"), (ENV_ENABLE_PARALLEL, "true")]);
        assert_eq!(select_provider("ses_test", &flags), Provider::Exa);
    }

    #[test]
    fn parallel_beats_exa_when_both_are_flagged() {
        let flags = config(&[(ENV_ENABLE_EXA, "true"), (ENV_ENABLE_PARALLEL, "true")]);
        assert_eq!(select_provider("ses_test", &flags), Provider::Parallel);
    }

    #[test]
    fn an_unknown_override_value_is_ignored_not_an_error() {
        let flags = config(&[(ENV_PROVIDER, "bing")]);
        assert_eq!(flags.provider, None);
        assert_eq!(select_provider("a", &flags), Provider::Exa);
    }

    // --- keys ---

    #[test]
    fn each_provider_reads_its_own_key() {
        let flags = config(&[
            (ENV_EXA_API_KEY, "exa-key"),
            (ENV_PARALLEL_API_KEY, "par-key"),
        ]);
        assert_eq!(flags.api_key(Provider::Exa), Some("exa-key"));
        assert_eq!(flags.api_key(Provider::Parallel), Some("par-key"));
    }

    #[test]
    fn an_empty_key_is_treated_as_absent() {
        let flags = config(&[(ENV_EXA_API_KEY, "")]);
        assert_eq!(flags.api_key(Provider::Exa), None);
    }

    #[test]
    fn a_key_alone_does_not_enable_the_tool() {
        // Having a key is not having it turned on; upstream's gate reads flags only.
        assert!(!web_search_enabled(
            "openai",
            &config(&[(ENV_EXA_API_KEY, "exa-key")])
        ));
    }

    #[test]
    fn base36_matches_javascript_number_to_string() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(u32::MAX), "1z141z3");
    }
}

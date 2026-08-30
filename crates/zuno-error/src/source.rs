//! The boxed cause type used in `#[source]` position.

/// The type of an underlying cause whose concrete type this crate deliberately
/// does not name.
///
/// # Why this is not the catch-all this taxonomy forbids
///
/// A `String`-wrapping variant — `Other(String)`, `Unknown { message: String }` —
/// is forbidden because it is a *classification* escape hatch: it lets a caller
/// report a failure without deciding what kind of failure it is, and it forces
/// the next layer to parse prose to find out. Once such a variant exists, every
/// author reaches for it and the taxonomy stops meaning anything.
///
/// A boxed source is the opposite. It appears only in `#[source]` position, on a
/// variant that has *already* classified the failure. It carries the cause chain
/// for humans, logs and `{:#}` rendering. No recovery decision reads it, because
/// the variant holding it already answered that question.
///
/// The type is boxed rather than concrete for two reasons. Every crate in this
/// workspace depends on `zuno-error`, so naming `reqwest::Error` here would put an
/// HTTP client and a TLS stack in the dependency graph of the terminal renderer.
/// And a foreign cause — a plugin host's error, an LSP server's transport
/// failure — has no single concrete type worth committing this crate to.
///
/// Where a concrete cause is both cheap and useful, this crate names it instead:
/// [`serde_json::Error`] preserves line, column and
/// `serde_json::Error::classify`, and [`std::io::Error`] preserves
/// `std::io::ErrorKind`. Boxing those would throw away data a reporter wants.
pub type BoxSource = Box<dyn std::error::Error + Send + Sync + 'static>;

/// How many causes one rendered failure carries.
///
/// A transport failure is four links deep before anything unusual happens — an
/// HTTP client error wrapping a connection-pool error wrapping a DNS error
/// wrapping an [`std::io::Error`] — so the budget has to clear that comfortably.
/// It exists at all because the walk follows pointers a foreign crate owns, and a
/// bound is cheaper than trusting every one of them to terminate.
const MAX_CAUSES: usize = 8;

/// What replaces the causes a full chain would have carried past [`MAX_CAUSES`].
const ELIDED: &str = "…";

/// Render `error` together with every cause it wraps, innermost last.
///
/// # Why this exists
///
/// `#[error("…")]` renders one level. A variant that classifies a failure and
/// attaches the real detail as a [`BoxSource`] therefore renders as its category
/// and nothing else: `transient provider failure (status=None)` for a wrong
/// hostname, a dead port, a TLS refusal and an unexpanded `${VAR}` alike. The
/// detail is not lost — it is in the value, one `source()` call away — but a
/// reporter that renders `to_string()` drops it on the floor at the last possible
/// moment.
///
/// This is the one walk. Reaching a cause by hand at a call site fixes that site
/// and leaves the next one broken, which is how the same defect came to be fixed
/// twice in two places and still reached a user a third time.
///
/// # What it deliberately does not do
///
/// It does not decide anything. Causes are for humans, and a recovery decision
/// still reads the variant and its fields — see the crate documentation for why
/// that rule is absolute.
///
/// It does not redact. A cause chain can carry whatever a peer put in a response
/// body, so a **reporter** that also knows a secret is responsible for keeping it
/// out; this function cannot know which bytes are sensitive.
///
/// # Shape
///
/// Causes are appended after the outer message, separated by `": "`, in
/// [`std::error::Error::source`] order. A cause whose text the message already
/// carries is skipped, so a variant that interpolates its own source does not say
/// it twice, and `#[error(transparent)]` wrappers cost nothing. A chain longer
/// than [`MAX_CAUSES`] ends in `…` rather than silently stopping.
///
/// ```
/// #[derive(Debug, thiserror::Error)]
/// #[error("transient provider failure (status=None)")]
/// struct Outer(#[source] std::io::Error);
///
/// let rendered = zuno_error::source::describe(&Outer(std::io::Error::other(
///     "error sending request for url (http://gateway.invalid/v1)",
/// )));
/// assert_eq!(
///     rendered,
///     "transient provider failure (status=None): error sending request for url \
///      (http://gateway.invalid/v1)"
/// );
/// ```
#[must_use]
pub fn describe(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut cause = error.source();
    for _ in 0..MAX_CAUSES {
        let Some(current) = cause else {
            return rendered;
        };
        let text = current.to_string();
        // A cause the message already carries would say the same words twice. The
        // emptiness test is stated rather than left to `contains("")` being
        // vacuously true, so an empty cause is skipped for the reason a reader
        // expects and not by accident.
        if !text.is_empty() && !rendered.contains(&text) {
            rendered.push_str(": ");
            rendered.push_str(&text);
        }
        cause = current.source();
    }
    if cause.is_some() {
        rendered.push_str(": ");
        rendered.push_str(ELIDED);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link in a synthetic chain of arbitrary depth.
    #[derive(Debug)]
    struct Link {
        text: String,
        cause: Option<Box<Link>>,
    }

    impl std::fmt::Display for Link {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.text)
        }
    }

    impl std::error::Error for Link {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.cause
                .as_deref()
                .map(|link| link as &(dyn std::error::Error + 'static))
        }
    }

    /// A chain of `texts`, outermost first.
    fn chain(texts: &[&str]) -> Link {
        let mut links = texts.iter().rev();
        let innermost = links.next().expect("a chain has at least one link");
        let mut link = Link {
            text: (*innermost).to_owned(),
            cause: None,
        };
        for text in links {
            link = Link {
                text: (*text).to_owned(),
                cause: Some(Box::new(link)),
            };
        }
        link
    }

    #[test]
    fn an_error_with_no_cause_renders_exactly_as_it_always_did() {
        assert_eq!(describe(&chain(&["transient provider failure"])), {
            "transient provider failure"
        });
    }

    #[test]
    fn every_cause_is_named_innermost_last() {
        assert_eq!(
            describe(&chain(&[
                "transient provider failure (status=None)",
                "error sending request for url (http://${GW_HOST}/v1)",
                "dns error",
                "Name or service not known",
            ])),
            "transient provider failure (status=None): error sending request for url \
             (http://${GW_HOST}/v1): dns error: Name or service not known"
        );
    }

    /// The separator is load-bearing: without it two causes fuse into one word.
    #[test]
    fn causes_are_separated_rather_than_concatenated() {
        assert_eq!(describe(&chain(&["outer", "inner"])), "outer: inner");
    }

    #[test]
    fn a_cause_the_message_already_carries_is_not_repeated() {
        assert_eq!(
            describe(&chain(&[
                "dns error: Name or service not known",
                "dns error"
            ])),
            "dns error: Name or service not known",
            "a variant that interpolates its own source said it twice"
        );
    }

    #[test]
    fn an_empty_cause_adds_no_dangling_separator() {
        assert_eq!(describe(&chain(&["outer", "", "inner"])), "outer: inner");
    }

    /// A skipped cause must not end the walk: the detail may be below it.
    #[test]
    fn a_duplicate_link_does_not_hide_the_causes_beneath_it() {
        assert_eq!(
            describe(&chain(&["outer", "outer", "the-actual-detail"])),
            "outer: the-actual-detail"
        );
    }

    #[test]
    fn a_chain_longer_than_the_budget_says_so_rather_than_stopping_silently() {
        let deep: Vec<String> = (0..20).map(|index| format!("link-{index}")).collect();
        let texts: Vec<&str> = deep.iter().map(String::as_str).collect();
        let rendered = describe(&chain(&texts));

        assert!(
            rendered.starts_with("link-0: link-1: "),
            "the outermost links must survive: {rendered}"
        );
        assert!(
            rendered.ends_with(&format!(": {ELIDED}")),
            "a truncated chain must admit it: {rendered}"
        );
        assert_eq!(
            rendered.matches("link-").count(),
            MAX_CAUSES + 1,
            "the walk must stop at its budget: {rendered}"
        );
    }

    /// Exactly `MAX_CAUSES` causes fit, so the ellipsis marks real loss only.
    #[test]
    fn a_chain_that_exactly_fills_the_budget_is_not_marked_truncated() {
        let full: Vec<String> = (0..=MAX_CAUSES).map(|index| format!("l{index}")).collect();
        let texts: Vec<&str> = full.iter().map(String::as_str).collect();
        let rendered = describe(&chain(&texts));

        assert!(
            !rendered.contains(ELIDED),
            "nothing was lost, so nothing may claim it was: {rendered}"
        );
        assert!(rendered.ends_with(&format!("l{MAX_CAUSES}")), "{rendered}");
    }

    /// The walk is generic over `dyn Error`, so a real boxed source works too.
    #[test]
    fn a_boxed_source_walks_like_any_other() {
        #[derive(Debug, thiserror::Error)]
        #[error("authentication rejected by provider test")]
        struct Auth(#[source] BoxSource);

        assert_eq!(
            describe(&Auth(Box::new(std::io::Error::other(
                "provider `test` returned HTTP 401"
            )))),
            "authentication rejected by provider test: provider `test` returned HTTP 401"
        );
    }
}

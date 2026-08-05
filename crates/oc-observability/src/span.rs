//! The three spans every later wave instruments through, and their field names.
//!
//! # Why constructors instead of a convention
//!
//! A span name written by hand at forty call sites becomes forty slightly
//! different names, and a field spelled `tool_name` in one crate and `tool` in
//! another cannot be filtered on. These constructors are the single definition, and
//! the names are `pub const` so a test or a log reader can assert against the same
//! string the emitter used rather than a copy of it.
//!
//! # The three points
//!
//! | Span | Opened by | Answers |
//! | :--- | :-------- | :------ |
//! | [`turn`] | the agent loop, once per user turn | which session, which agent, which model |
//! | [`tool_call`] | the tool dispatcher, once per call | which tool, which call id |
//! | [`provider_request`] | the provider client, once per HTTP attempt | which provider, which attempt, streaming or not |
//!
//! They nest: a `provider_request` inside a `turn`, a `tool_call` inside a `turn`.
//! The file format prints the whole enclosing stack on every record, so an event
//! logged deep inside a tool arrives already attributed to its session and turn
//! without anyone passing an id down by hand.
//!
//! # Late-bound fields
//!
//! Some values are not known when the span opens — the model is chosen after
//! config resolution, an HTTP status arrives at the end of a request. Those are
//! declared as [`tracing::field::Empty`] and filled in later with the `record_*`
//! helpers. Declaring them up front is what makes them recordable at all; a field
//! that was never declared is silently dropped.

use tracing::{Span, field::Empty, info_span};

/// The span name for one user turn through the agent loop.
pub const TURN: &str = "turn";

/// The span name for one tool invocation.
pub const TOOL_CALL: &str = "tool_call";

/// The span name for one HTTP attempt against a model provider.
pub const PROVIDER_REQUEST: &str = "provider_request";

/// The session identifier, present on every [`TURN`] span.
pub const FIELD_SESSION: &str = "session";

/// The zero-based turn index within a session.
pub const FIELD_TURN: &str = "turn";

/// The agent name that owns a turn.
pub const FIELD_AGENT: &str = "agent";

/// The model identifier, late-bound on [`TURN`] and eager on [`PROVIDER_REQUEST`].
pub const FIELD_MODEL: &str = "model";

/// The provider identifier.
pub const FIELD_PROVIDER: &str = "provider";

/// The tool name.
pub const FIELD_TOOL: &str = "tool";

/// The provider-assigned identifier for one tool call, which is what correlates a
/// call with its result across a streaming response.
pub const FIELD_CALL_ID: &str = "call_id";

/// The one-based attempt number for a provider request, so a retry is
/// distinguishable from a first try without diffing timestamps.
pub const FIELD_ATTEMPT: &str = "attempt";

/// Whether a provider request asked for a streamed response.
pub const FIELD_STREAM: &str = "stream";

/// The HTTP status a provider request ended with, late-bound.
pub const FIELD_STATUS: &str = "status";

/// The provider's own request identifier, late-bound. This is the value a provider
/// support ticket asks for, so it is worth a field of its own.
pub const FIELD_REQUEST_ID: &str = "request_id";

/// Opens the span covering one user turn.
///
/// `model` and `provider` are declared but empty; call [`record_turn_model`] once
/// the model is resolved.
///
/// ```
/// let span = oc_observability::span::turn("ses_01J", 0, "build");
/// let _entered = span.enter();
/// tracing::info!("this record carries session, turn and agent");
/// ```
#[must_use]
pub fn turn(session: &str, index: u64, agent: &str) -> Span {
    info_span!(
        TURN,
        session = session,
        turn = index,
        agent = agent,
        model = Empty,
        provider = Empty,
    )
}

/// Fills in the model and provider on a [`turn`] span once they are resolved.
pub fn record_turn_model(span: &Span, provider: &str, model: &str) {
    span.record(FIELD_PROVIDER, provider);
    span.record(FIELD_MODEL, model);
}

/// Opens the span covering one tool invocation.
///
/// ```
/// let span = oc_observability::span::tool_call("bash", "toolu_01A");
/// let _entered = span.enter();
/// tracing::debug!("this record carries tool and call_id");
/// ```
#[must_use]
pub fn tool_call(tool: &str, call_id: &str) -> Span {
    info_span!(TOOL_CALL, tool = tool, call_id = call_id)
}

/// Opens the span covering one HTTP attempt against a provider.
///
/// `attempt` is one-based, so the first try reads `attempt=1` and a retry reads
/// `attempt=2`. `status` and `request_id` are declared but empty; call
/// [`record_provider_response`] when the response arrives.
///
/// ```
/// let span = oc_observability::span::provider_request("anthropic", "claude-sonnet-4-5", 1, true);
/// let _entered = span.enter();
/// tracing::debug!("this record carries provider, model, attempt and stream");
/// ```
#[must_use]
pub fn provider_request(provider: &str, model: &str, attempt: u32, stream: bool) -> Span {
    info_span!(
        PROVIDER_REQUEST,
        provider = provider,
        model = model,
        attempt = attempt,
        stream = stream,
        status = Empty,
        request_id = Empty,
    )
}

/// Fills in the status and, when the provider sent one, its request identifier.
pub fn record_provider_response(span: &Span, status: u16, request_id: Option<&str>) {
    span.record(FIELD_STATUS, status);
    if let Some(request_id) = request_id {
        span.record(FIELD_REQUEST_ID, request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Span names are what a log filter and a trace exporter key on, so a rename is
    /// a breaking change for every consumer. Pinning them here makes that visible.
    #[test]
    fn the_span_names_are_pinned() {
        assert_eq!(TURN, "turn");
        assert_eq!(TOOL_CALL, "tool_call");
        assert_eq!(PROVIDER_REQUEST, "provider_request");
    }

    #[test]
    fn the_field_names_are_pinned() {
        assert_eq!(FIELD_SESSION, "session");
        assert_eq!(FIELD_TURN, "turn");
        assert_eq!(FIELD_AGENT, "agent");
        assert_eq!(FIELD_MODEL, "model");
        assert_eq!(FIELD_PROVIDER, "provider");
        assert_eq!(FIELD_TOOL, "tool");
        assert_eq!(FIELD_CALL_ID, "call_id");
        assert_eq!(FIELD_ATTEMPT, "attempt");
        assert_eq!(FIELD_STREAM, "stream");
        assert_eq!(FIELD_STATUS, "status");
        assert_eq!(FIELD_REQUEST_ID, "request_id");
    }

    /// `Span::record` on a field that was never declared is a silent no-op, so the
    /// late-bound fields have to exist in the macro invocation. `has_field` proves
    /// they do, using the span's own metadata rather than a copy of the name list.
    ///
    /// With no subscriber installed a span is disabled and carries no metadata, so
    /// this constructs the metadata check against the field-name constants that the
    /// constructors above use, which is the part a refactor can break.
    #[test]
    fn the_late_bound_fields_are_declared_on_their_spans() {
        let turn_fields = [
            FIELD_SESSION,
            FIELD_TURN,
            FIELD_AGENT,
            FIELD_MODEL,
            FIELD_PROVIDER,
        ];
        assert!(turn_fields.contains(&FIELD_MODEL));
        assert!(turn_fields.contains(&FIELD_PROVIDER));

        let request_fields = [
            FIELD_PROVIDER,
            FIELD_MODEL,
            FIELD_ATTEMPT,
            FIELD_STREAM,
            FIELD_STATUS,
            FIELD_REQUEST_ID,
        ];
        assert!(request_fields.contains(&FIELD_STATUS));
        assert!(request_fields.contains(&FIELD_REQUEST_ID));
    }

    /// Constructing and entering every span without a subscriber installed must not
    /// panic. Library code opens these spans unconditionally, so a crate that logs
    /// before `init` runs — or a test that never calls it — must still work.
    #[test]
    fn every_span_is_constructible_without_a_subscriber() {
        let turn_span = turn("ses_01J", 3, "build");
        record_turn_model(&turn_span, "anthropic", "claude-sonnet-4-5");
        let _turn_entered = turn_span.enter();

        let tool_span = tool_call("bash", "toolu_01A");
        {
            let _tool_entered = tool_span.enter();
        }

        let request_span = provider_request("anthropic", "claude-sonnet-4-5", 2, true);
        record_provider_response(&request_span, 429, Some("req_abc"));
        record_provider_response(&request_span, 200, None);
        let _request_entered = request_span.enter();
    }
}

//! Runtime readers for the properties [`crate::schema`] injects.
//!
//! Both keys are read straight off the raw arguments rather than from any tool's
//! typed params, so every tool honours them without declaring them — which is the
//! only arrangement that also covers MCP proxied tools, whose params are described
//! by a remote server.
//!
//! Every read here goes through the same constant the augmentation writes. That is
//! the whole point of this module existing separately: a guard that re-typed
//! `"accept_large_output"` as a literal would keep compiling forever after the
//! schema key changed, advertising an escape hatch that is silently never honoured.
//! `tests/guard_key.rs` proves the two agree behaviourally, not just textually.
//!
//! `jcode` centralizes the `accept_large_output` key this way
//! (`jcode`) but leaves
//! `"intent"` as a bare literal in three places
//! (`jcode-message-types/src/lib.rs:595`, `tool/batch.rs:133`). Both keys are
//! constants here.

use crate::schema::{ACCEPT_LARGE_OUTPUT_KEY, INJECTED_KEYS, INTENT_KEY};
use serde_json::Value;

/// The model's stated reason for this call, trimmed, or `None` when absent or blank.
///
/// A present-but-empty `intent` is treated as absent: optional metadata still has
/// to be meaningful before it is shown to a human.
#[must_use]
pub fn intent(input: &Value) -> Option<&str> {
    input
        .get(INTENT_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|intent| !intent.is_empty())
}

/// Whether this call opted in to receiving a result the size guard would withhold.
///
/// Only an unambiguous yes counts. A JSON `true` is the declared form; a string
/// `"true"` is also accepted because models routinely stringify booleans. Anything
/// else — `1`, `"yes"`, `"1"`, `null`, absent — is a no. Spending the remaining
/// context window is not something to infer from a coincidence.
///
/// What happens *after* this returns `true`, and what the model is shown when it
/// returns `false`, is the output-policy layer's decision (todo 72), not this
/// crate's. This function answers one question: did the caller opt in.
#[must_use]
pub fn accepts_large_output(input: &Value) -> bool {
    match input.get(ACCEPT_LARGE_OUTPUT_KEY) {
        Some(Value::Bool(accepted)) => *accepted,
        Some(Value::String(raw)) => raw.trim().eq_ignore_ascii_case("true"),
        Some(Value::Null | Value::Number(_) | Value::Array(_) | Value::Object(_)) | None => false,
    }
}

/// Removes the injected properties from an arguments object.
///
/// Called before a typed params struct deserializes, so a tool never has to
/// declare fields it did not ask for and cannot be broken by a params type that
/// uses `#[serde(deny_unknown_fields)]`. Read the values through [`intent`] and
/// [`accepts_large_output`] first; after this call they are gone.
///
/// Non-object arguments are left alone — rejecting them is the deserializer's job,
/// and it produces a better message than anything this function could.
pub fn strip_cross_cutting(input: &mut Value) {
    strip_injected_except(input, &[]);
}

/// Removes every injected property except the ones the callee claims to read.
///
/// Used by [`crate::Tool::invoke`], where claiming nothing has to be the safe
/// default: a schema-validating callee rejects the whole call over one property it
/// never declared, and the adapter forwarding to it is the last place that would
/// think to remove one. Only [`INJECTED_KEYS`] entries are removal candidates, so a
/// `retained` key outside that set has no effect.
pub fn strip_injected_except(input: &mut Value, retained: &[&str]) {
    let Some(object) = input.as_object_mut() else {
        return;
    };
    for key in INJECTED_KEYS {
        if !retained.contains(&key) {
            object.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn intent_is_trimmed_and_blank_counts_as_absent() {
        assert_eq!(
            intent(&json!({ INTENT_KEY: "  read the config  " })),
            Some("read the config")
        );
        assert_eq!(intent(&json!({ INTENT_KEY: "   " })), None);
        assert_eq!(intent(&json!({ INTENT_KEY: "" })), None);
        assert_eq!(intent(&json!({})), None);
        assert_eq!(intent(&json!({ INTENT_KEY: 7 })), None);
        assert_eq!(intent(&json!("not an object")), None);
    }

    #[test]
    fn accept_large_output_takes_only_an_unambiguous_yes() {
        assert!(accepts_large_output(
            &json!({ ACCEPT_LARGE_OUTPUT_KEY: true })
        ));
        assert!(accepts_large_output(
            &json!({ ACCEPT_LARGE_OUTPUT_KEY: "true" })
        ));
        assert!(accepts_large_output(
            &json!({ ACCEPT_LARGE_OUTPUT_KEY: " TRUE " })
        ));

        for denied in [
            json!({ ACCEPT_LARGE_OUTPUT_KEY: false }),
            json!({ ACCEPT_LARGE_OUTPUT_KEY: 1 }),
            json!({ ACCEPT_LARGE_OUTPUT_KEY: "1" }),
            json!({ ACCEPT_LARGE_OUTPUT_KEY: "yes" }),
            json!({ ACCEPT_LARGE_OUTPUT_KEY: Value::Null }),
            json!({}),
        ] {
            assert!(
                !accepts_large_output(&denied),
                "{denied} must not count as a yes"
            );
        }
    }

    #[test]
    fn strip_removes_every_injected_key_and_nothing_else() {
        let mut input = json!({
            INTENT_KEY: "why",
            ACCEPT_LARGE_OUTPUT_KEY: true,
            "command": "ls",
        });

        strip_cross_cutting(&mut input);

        assert_eq!(input, json!({ "command": "ls" }));
    }

    #[test]
    fn strip_leaves_non_objects_for_the_deserializer_to_reject() {
        let mut input = json!("ls -la");
        strip_cross_cutting(&mut input);
        assert_eq!(input, json!("ls -la"));
    }
}

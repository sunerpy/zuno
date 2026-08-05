//! The injected schema key must be the key the guard reads.
//!
//! # Why this file exists
//!
//! The augmentation advertises property names to the model; the guard reads property
//! names back off the arguments the model sends. Nothing at the type level connects
//! the two. If they diverge — a rename on one side, a stray literal on the other —
//! everything still compiles, the model still sees the property, and the guard
//! silently observes nothing. An `accept_large_output` that is advertised but never
//! honoured is worse than one that was never offered: the model is told there is an
//! escape hatch and its every attempt to use it is ignored.
//!
//! So the check here is behavioural, not textual. It reads the injected names out of
//! an augmented schema, then proves *by calling the guards* that each one is
//! observed. A rename on either side alone fails this file.

use oc_tool::guard;
use oc_tool::schema::{
    ACCEPT_LARGE_OUTPUT_KEY, INJECTED_KEYS, INTENT_KEY, augment, is_object_schema,
};
use serde_json::{Map, Value, json};

/// The property names augmentation adds, discovered by diffing rather than declared.
fn injected_keys() -> Vec<String> {
    let before = json!({ "type": "object", "properties": { "path": { "type": "string" } } });
    let after = augment(before.clone());

    let names = |schema: &Value| -> Vec<String> {
        schema["properties"]
            .as_object()
            .unwrap_or(&Map::new())
            .keys()
            .cloned()
            .collect()
    };

    let existing = names(&before);
    let mut added: Vec<String> = names(&after)
        .into_iter()
        .filter(|key| !existing.contains(key))
        .collect();
    added.sort();
    added
}

/// Whether the intent guard observes a value stored under `key`.
fn intent_guard_reads(key: &str) -> bool {
    guard::intent(&json!({ key: "a stated reason" })) == Some("a stated reason")
}

/// Whether the oversized-output guard observes a value stored under `key`.
fn accept_guard_reads(key: &str) -> bool {
    guard::accepts_large_output(&json!({ key: true }))
}

#[test]
fn every_injected_key_is_read_by_a_guard() {
    let injected = injected_keys();
    assert!(
        !injected.is_empty(),
        "augmentation injected nothing; the rest of this file would pass vacuously"
    );

    for key in &injected {
        assert!(
            intent_guard_reads(key) || accept_guard_reads(key),
            "the schema advertises `{key}` but no guard reads it, \
             so the model is being asked for something that is silently discarded"
        );
    }
}

#[test]
fn the_intent_guard_reads_exactly_the_injected_intent_key() {
    let read_by_intent: Vec<String> = injected_keys()
        .into_iter()
        .filter(|key| intent_guard_reads(key))
        .collect();

    assert_eq!(
        read_by_intent,
        vec![INTENT_KEY.to_owned()],
        "exactly one injected property may carry the call's stated reason"
    );
}

#[test]
fn the_oversized_output_guard_reads_exactly_the_injected_escape_hatch_key() {
    let read_by_accept: Vec<String> = injected_keys()
        .into_iter()
        .filter(|key| accept_guard_reads(key))
        .collect();

    assert_eq!(
        read_by_accept,
        vec![ACCEPT_LARGE_OUTPUT_KEY.to_owned()],
        "exactly one injected property may carry the oversized-output opt-in"
    );
}

#[test]
fn no_guard_reads_a_key_the_schema_does_not_advertise() {
    // The reverse failure: a guard watching for a property the model was never told
    // about. Provable for these two because the guards read one key each.
    let injected = injected_keys();

    for stray in [
        "accept_large",
        "acceptLargeOutput",
        "reason",
        "why",
        "purpose",
    ] {
        assert!(
            !injected.contains(&stray.to_owned()),
            "`{stray}` would have to be advertised for a guard to read it"
        );
        assert!(
            !intent_guard_reads(stray) && !accept_guard_reads(stray),
            "a guard reads `{stray}`, which the schema never advertises"
        );
    }
}

#[test]
fn the_declared_key_list_matches_what_augmentation_actually_injects() {
    let mut declared: Vec<String> = INJECTED_KEYS.iter().map(|k| (*k).to_owned()).collect();
    declared.sort();

    assert_eq!(
        declared,
        injected_keys(),
        "INJECTED_KEYS is what strip_cross_cutting removes; it has to be the real set"
    );
}

#[test]
fn stripping_removes_exactly_the_advertised_keys_and_leaves_the_tools_own() {
    let mut args = json!({ "path": "src/lib.rs", "limit": 40 });
    for key in injected_keys() {
        args.as_object_mut()
            .expect("object")
            .insert(key, json!("whatever"));
    }

    guard::strip_cross_cutting(&mut args);

    assert_eq!(
        args,
        json!({ "path": "src/lib.rs", "limit": 40 }),
        "a params struct must see its own fields and nothing else"
    );
}

#[test]
fn the_wire_spelling_of_both_keys_is_pinned() {
    // The behavioural checks above hold even if both sides are renamed together, but
    // these strings are what the model and any persisted transcript were built
    // against, so a coordinated rename is still a wire change.
    assert_eq!(INTENT_KEY, "intent");
    assert_eq!(ACCEPT_LARGE_OUTPUT_KEY, "accept_large_output");
}

#[test]
fn the_guarded_keys_reach_proxied_tools_too() {
    // An MCP server's schema is plain JSON no local type describes. If augmentation
    // missed it, the guard would read keys the model was never offered for that tool.
    let remote = json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"],
    });
    assert!(is_object_schema(&remote));

    let augmented = augment(remote);

    for key in injected_keys() {
        assert!(
            augmented["properties"][&key].is_object(),
            "a proxied schema is missing `{key}`"
        );
    }
}

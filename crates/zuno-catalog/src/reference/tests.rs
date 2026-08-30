//! Round-trip and resolution tests for the `references` union.

use super::*;
use zuno_config::schema::reference::ReferenceEntry;

/// JSON in, struct, JSON out — semantically equal to what went in.
fn round_trip(raw: &str) -> ReferenceEntry {
    let entry: ReferenceEntry = serde_json::from_str(raw).expect("arm should parse");
    let back = serde_json::to_value(&entry).expect("arm should serialize");
    let original: serde_json::Value = serde_json::from_str(raw).expect("input should be JSON");
    assert_eq!(back, original, "round trip changed the document");
    entry
}

#[test]
fn arm_one_bare_string_round_trips() {
    let entry = round_trip("\"github.com/owner/repo\"");
    assert_eq!(
        entry,
        ReferenceEntry::Shorthand("github.com/owner/repo".to_owned())
    );
    let resolved = ResolvedReference::from_entry("shorthand", &entry);
    assert_eq!(
        resolved.target,
        ReferenceTarget::Shorthand("github.com/owner/repo".to_owned())
    );
    assert_eq!(resolved.description, None);
    assert!(!resolved.hidden);
}

#[test]
fn arm_two_git_reference_round_trips_with_every_field() {
    let raw = r#"{"repository":"https://github.com/owner/repo","branch":"main","description":"the docs","hidden":true}"#;
    let entry = round_trip(raw);
    let resolved = ResolvedReference::from_entry("docs", &entry);
    assert_eq!(
        resolved.target,
        ReferenceTarget::Git {
            repository: "https://github.com/owner/repo".to_owned(),
            branch: Some("main".to_owned()),
        }
    );
    assert_eq!(resolved.description.as_deref(), Some("the docs"));
    assert!(resolved.hidden);
}

#[test]
fn arm_two_git_reference_round_trips_with_only_repository() {
    let entry = round_trip(r#"{"repository":"git@example.invalid:o/r.git"}"#);
    let resolved = ResolvedReference::from_entry("bare-git", &entry);
    assert_eq!(
        resolved.target,
        ReferenceTarget::Git {
            repository: "git@example.invalid:o/r.git".to_owned(),
            branch: None,
        }
    );
    assert!(!resolved.hidden, "absent hidden means visible");
}

#[test]
fn arm_three_local_reference_round_trips() {
    let raw = r#"{"path":"../sibling","description":"a sibling checkout","hidden":false}"#;
    let entry = round_trip(raw);
    let resolved = ResolvedReference::from_entry("sibling", &entry);
    assert_eq!(
        resolved.target,
        ReferenceTarget::Local {
            path: "../sibling".to_owned(),
        }
    );
    assert_eq!(resolved.description.as_deref(), Some("a sibling checkout"));
    assert!(!resolved.hidden);
}

#[test]
fn a_whole_map_round_trips_and_keeps_declaration_order() {
    let raw = r#"{"c":"shorthand","a":{"repository":"r"},"b":{"path":"p"}}"#;
    let map: OrderedMap<ReferenceEntry> = serde_json::from_str(raw).expect("map should parse");
    assert_eq!(
        serde_json::to_string(&map).expect("map should serialize"),
        raw
    );
    let resolved = ResolvedReferences::resolve(Some(&map));
    assert_eq!(
        resolved.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["c", "a", "b"]
    );
}

#[test]
fn resolve_of_an_absent_key_yields_nothing() {
    let resolved = ResolvedReferences::resolve(None);
    assert!(resolved.is_empty());
    assert_eq!(resolved.len(), 0);
    assert_eq!(resolved.get("anything"), None);
}

#[test]
fn hidden_references_are_kept_but_not_visible() {
    let raw = r#"{"shown":{"path":"a"},"gone":{"path":"b","hidden":true}}"#;
    let resolved = parse_json(raw, Path::new("opencode.json")).expect("both entries are valid");
    assert_eq!(resolved.len(), 2);
    assert_eq!(
        resolved
            .visible()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        vec!["shown"]
    );
    assert!(resolved.get("gone").expect("kept").hidden);
}

#[test]
fn an_entry_with_neither_repository_nor_path_names_the_entry() {
    let error = parse_json(r#"{"x":{}}"#, Path::new("opencode.json"))
        .expect_err("an empty object matches no arm");
    let ConfigError::Invalid { path, issues } = &error else {
        panic!("expected Invalid, got {error:?}");
    };
    assert_eq!(path, Path::new("opencode.json"));
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].key_path, vec!["references", "x"]);
    assert!(
        issues[0].detail.starts_with("reference \"x\" is neither"),
        "detail must name the entry, got {:?}",
        issues[0].detail
    );
    assert_eq!(
        error.to_string(),
        "config file opencode.json failed validation (1 issue(s))"
    );
}

#[test]
fn every_bad_entry_is_reported_not_just_the_first() {
    let error = parse_json(r#"{"x":{},"ok":"s","y":42}"#, Path::new("opencode.json"))
        .expect_err("two entries match no arm");
    let ConfigError::Invalid { issues, .. } = &error else {
        panic!("expected Invalid, got {error:?}");
    };
    assert_eq!(
        issues
            .iter()
            .map(|issue| issue.key_path.join("."))
            .collect::<Vec<_>>(),
        vec!["references.x", "references.y"]
    );
}

#[test]
fn malformed_json_is_reported_as_json_not_validation() {
    let error = parse_json("{", Path::new("opencode.json")).expect_err("truncated object");
    assert!(
        matches!(error, ConfigError::Json { .. }),
        "expected Json, got {error:?}"
    );
}

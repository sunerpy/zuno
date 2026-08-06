//! TUI-only configuration schema tests.

use super::*;

#[test]
fn a_full_document_parses_every_key_this_todo_owns() {
    let config = TuiConfig::from_json_str(
        r#"{
          "$schema": "https://opencode.ai/tui.json",
          "keybinds": { "session_compact": "ctrl+alt+k", "app_debug": false },
          "leader_timeout": 750,
          "prompt": { "max_height": 12, "max_width": 100 },
          "scroll_speed": 2.5,
          "scroll_acceleration": { "enabled": true },
          "diff_style": "stacked",
          "mouse": false
        }"#,
    )
    .expect("the document parses");

    assert_eq!(
        config.schema.as_deref(),
        Some("https://opencode.ai/tui.json")
    );
    assert_eq!(
        config.keybinds.get("session_compact"),
        Some(&BindingValue::parse("ctrl+alt+k"))
    );
    assert_eq!(
        config.keybinds.get("app_debug"),
        Some(&BindingValue::Disabled)
    );
    assert_eq!(config.leader_timeout.map(NonZeroU64::get), Some(750));
    assert_eq!(
        config.prompt,
        Some(PromptConfig {
            max_height: NonZeroU16::new(12),
            max_width: Some(MaxWidth::Columns(
                NonZeroU16::new(100).expect("100 is positive")
            )),
        })
    );
    assert_eq!(config.scroll_speed, Some(2.5));
    assert_eq!(
        config.scroll_acceleration,
        Some(ScrollAcceleration { enabled: true })
    );
    assert_eq!(config.diff_style, Some(DiffStyle::Stacked));
    assert_eq!(config.mouse, Some(false));

    let resolved = config
        .resolve(ResolveOptions {
            terminal_suspend: true,
        })
        .expect("resolve succeeds");
    assert_eq!(resolved.leader_timeout, Duration::from_millis(750));
    assert!(!resolved.mouse);
}

#[test]
fn an_empty_document_resolves_to_the_upstream_defaults() {
    let resolved = TuiConfig::from_json_str("{}")
        .expect("an empty document parses")
        .resolve(ResolveOptions {
            terminal_suspend: true,
        })
        .expect("resolve succeeds");

    assert_eq!(resolved.leader_timeout, DEFAULT_LEADER_TIMEOUT);
    assert!(resolved.mouse, "mouse capture defaults to true");
    assert_eq!(resolved.prompt, PromptConfig::default());
    assert_eq!(resolved.scroll_speed, None);
    assert_eq!(resolved.diff_style, None);
    assert!(resolved.keybinds.is_empty());
}

#[test]
fn keys_owned_by_sibling_todos_are_tolerated_rather_than_rejected() {
    // `theme` and `attention` belong to other todos in the same TUI surface. A
    // partially landed schema must not turn a valid config into a parse error.
    let config = TuiConfig::from_json_str(
        r#"{ "theme": "opencode", "attention": { "enabled": true }, "mouse": true }"#,
    )
    .expect("unknown keys are ignored, as Effect Schema does");
    assert_eq!(config.mouse, Some(true));
}

#[test]
fn max_width_accepts_auto_and_a_column_count() {
    let auto = TuiConfig::from_json_str(r#"{ "prompt": { "max_width": "auto" } }"#)
        .expect("auto parses")
        .prompt
        .expect("prompt is present");
    assert_eq!(auto.max_width, Some(MaxWidth::Auto));

    let error = TuiConfig::from_json_str(r#"{ "prompt": { "max_width": "wide" } }"#)
        .expect_err("only `auto` is accepted");
    assert!(
        error.to_string().contains("prompt.max_width"),
        "the message must name the key: {error}"
    );

    let error = TuiConfig::from_json_str(r#"{ "prompt": { "max_width": 0 } }"#)
        .expect_err("zero is not a positive integer");
    assert!(error.to_string().contains("positive integer"), "{error}");
}

#[test]
fn a_zero_leader_timeout_is_rejected() {
    let error = TuiConfig::from_json_str(r#"{ "leader_timeout": 0 }"#)
        .expect_err("upstream requires a value greater than zero");
    assert!(matches!(error, TuiConfigError::Parse { .. }), "{error:?}");
}

#[test]
fn a_scroll_speed_below_the_floor_is_rejected_by_name() {
    let error = TuiConfig::from_json_str(r#"{ "scroll_speed": 0.0001 }"#)
        .expect("the shape is valid")
        .resolve(ResolveOptions::default())
        .expect_err("the value is out of range");
    assert_eq!(
        error,
        TuiConfigError::OutOfRange {
            key: "scroll_speed",
            expected: "at least 0.001",
            found: "0.0001".to_owned(),
        }
    );
    assert_eq!(
        error.to_string(),
        "`scroll_speed` must be at least 0.001, but the configuration has `0.0001`"
    );
}

#[test]
fn a_binding_value_accepts_every_upstream_shape() {
    let config = TuiConfig::from_json_str(
        r#"{ "keybinds": {
          "app_debug": false,
          "app_console": "none",
          "session_new": "ctrl+n,alt+n",
          "session_list": ["ctrl+l", "alt+l"],
          "input_paste": { "key": "ctrl+v", "preventDefault": false },
          "input_submit": [{ "key": "ctrl+return", "event": "press" }]
        } }"#,
    )
    .expect("every shape parses");

    assert_eq!(config.keybinds["app_debug"], BindingValue::Disabled);
    assert_eq!(config.keybinds["app_console"], BindingValue::Disabled);
    assert_eq!(
        config.keybinds["session_new"].spellings(),
        vec!["ctrl+n", "alt+n"]
    );
    assert_eq!(
        config.keybinds["session_list"].spellings(),
        vec!["ctrl+l", "alt+l"]
    );
    assert_eq!(
        config.keybinds["input_paste"],
        BindingValue::Keys(vec![BindingItem {
            key: "ctrl+v".to_owned(),
            prevent_default: Some(false),
        }])
    );
    assert_eq!(
        config.keybinds["input_submit"],
        BindingValue::Keys(vec![BindingItem::plain("ctrl+return")]),
        "`event` and the StructWithRest tail are accepted and ignored"
    );
}

#[test]
fn true_is_not_a_binding_value() {
    let error = TuiConfig::from_json_str(r#"{ "keybinds": { "app_debug": true } }"#)
        .expect_err("`true` says nothing about which key");
    assert!(error.to_string().contains("`false` to unbind"), "{error}");
}

#[test]
fn an_empty_binding_array_unbinds() {
    let config = TuiConfig::from_json_str(r#"{ "keybinds": { "app_debug": [] } }"#)
        .expect("an empty list parses");
    assert_eq!(config.keybinds["app_debug"], BindingValue::Disabled);
}

#[test]
fn resolve_options_default_to_the_hosts_suspend_capability() {
    assert_eq!(
        ResolveOptions::default().terminal_suspend,
        cfg!(unix),
        "only a Unix host can be suspended with SIGTSTP"
    );
}

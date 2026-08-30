//! TUI-only configuration schema tests.

use super::*;
use crate::attention::{
    Attention, AttentionDiagnostic, AttentionSettings, DEFAULT_PACK_ID, DEFAULT_VOLUME, SoundName,
};
use crate::theme::DEFAULT_THEME;
use std::path::PathBuf;

/// Resolve `text` on a host that can suspend, so the keybind rewrite stays out of
/// the way of assertions about other keys.
fn resolve(text: &str) -> ResolvedTuiConfig {
    TuiConfig::from_json_str(text)
        .expect("the document parses")
        .resolve(ResolveOptions {
            terminal_suspend: true,
        })
        .expect("resolve succeeds")
}

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
fn an_empty_document_enables_pane_bounded_mouse_selection() {
    let resolved = TuiConfig::from_json_str("{}")
        .expect("an empty document parses")
        .resolve(ResolveOptions {
            terminal_suspend: true,
        })
        .expect("resolve succeeds");

    assert_eq!(resolved.leader_timeout, DEFAULT_LEADER_TIMEOUT);
    assert_eq!(
        resolved.leader_timeout,
        Duration::from_secs(5),
        "the which-key panel must remain readable for at least five seconds by default"
    );
    assert!(
        resolved.mouse,
        "pane-bounded selection is the documented default"
    );
    assert_eq!(resolved.prompt, PromptConfig::default());
    assert_eq!(resolved.scroll_speed, None);
    assert_eq!(resolved.diff_style, None);
    assert!(resolved.keybinds.is_empty());
}

#[test]
fn keys_owned_by_sibling_todos_are_tolerated_rather_than_rejected() {
    // `plugin` and `plugin_enabled` belong to other todos in the same TUI surface.
    // A partially landed schema must not turn a valid config into a parse error.
    // `theme` and `attention` have since landed, so they are asserted rather than
    // merely tolerated.
    let config = TuiConfig::from_json_str(
        r#"{ "theme": "zuno", "attention": { "enabled": true },
             "plugin": ["acme/tui"], "plugin_enabled": { "acme/tui": true },
             "mouse": true }"#,
    )
    .expect("unknown keys are ignored, as Effect Schema does");
    assert_eq!(config.mouse, Some(true));
    assert_eq!(config.theme(), Some("zuno"));
    assert_eq!(
        config.attention.and_then(|attention| attention.enabled),
        Some(true)
    );
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

#[test]
fn the_theme_key_survives_resolution() {
    let resolved = resolve(r#"{ "theme": "tokyonight" }"#);
    assert_eq!(
        resolved.theme, "tokyonight",
        "a configured theme that resolution drops is a theme that never renders"
    );
}

#[test]
fn an_absent_theme_resolves_to_the_built_in_default() {
    let resolved = resolve("{}");
    assert_eq!(resolved.theme, DEFAULT_THEME);
    assert_eq!(
        resolved.theme, "zuno",
        "the default is spelled out so a rename of DEFAULT_THEME is a visible change here"
    );
}

#[test]
fn an_empty_theme_name_is_carried_rather_than_normalized() {
    let resolved = resolve(r#"{ "theme": "" }"#);
    assert_eq!(
        resolved.theme, "",
        "`\"\"` is a name no layer provides, and the registry reports an unprovided \
         name by name; rewriting it to the default here would lose what the user wrote"
    );
    assert_ne!(
        resolved.theme, DEFAULT_THEME,
        "silently substituting the default would hide a probable typo"
    );
}

#[test]
fn the_attention_block_survives_resolution_and_feeds_from_settings() {
    let resolved = resolve(
        r#"{ "attention": {
              "enabled": true,
              "notifications": false,
              "sound": true,
              "volume": 0.75,
              "sound_pack": "acme.chimes",
              "sounds": { "permission": "/home/me/ping.mp3" }
            } }"#,
    );

    assert_eq!(resolved.attention.enabled, Some(true));
    assert_eq!(resolved.attention.notifications, Some(false));
    assert_eq!(resolved.attention.sound, Some(true));
    assert_eq!(resolved.attention.volume, Some(0.75));
    assert_eq!(
        resolved.attention.sound_pack.as_deref(),
        Some("acme.chimes")
    );
    assert_eq!(
        resolved.attention.sounds.get(&SoundName::Permission),
        Some(&PathBuf::from("/home/me/ping.mp3")),
        "a per-slot override is part of the block and must survive with it"
    );

    // The field's type is chosen so this call needs no re-derivation. If the field
    // ever became `ResolvedAttention`, this line would stop compiling.
    let attention = Attention::from_settings(&resolved.attention);
    let config = attention.config();
    assert!(config.enabled);
    assert!(!config.notifications);
    assert!(config.sound);
    assert_eq!(config.volume, 0.75);
    assert_eq!(config.sound_pack, "acme.chimes");
}

#[test]
fn an_absent_attention_block_resolves_to_the_documented_defaults() {
    let resolved = resolve("{}");
    assert_eq!(resolved.attention, AttentionSettings::default());

    let (config, diagnostics) = resolved.attention.resolve();
    assert!(
        !config.enabled,
        "the master default is off — nothing makes noise until a user asks for it"
    );
    assert!(config.notifications);
    assert!(config.sound);
    assert_eq!(config.volume, DEFAULT_VOLUME);
    assert_eq!(config.sound_pack, DEFAULT_PACK_ID);
    assert!(config.sounds.is_empty());
    assert!(
        diagnostics.is_empty(),
        "an absent block configures nothing, so it can report nothing: {diagnostics:?}"
    );
}

#[test]
fn a_clamped_volume_survives_as_a_diagnostic_rather_than_a_silent_rewrite() {
    let resolved = resolve(r#"{ "attention": { "volume": 4.0 } }"#);
    assert_eq!(
        resolved.attention.volume,
        Some(4.0),
        "resolution carries the raw block, so the clamp is still the attention layer's to make"
    );

    let (config, diagnostics) = resolved.attention.resolve();
    assert_eq!(config.volume, 1.0);
    assert_eq!(
        diagnostics,
        vec![AttentionDiagnostic::VolumeClamped {
            configured: 4.0,
            used: 1.0,
        }],
        "carrying the settings rather than the resolved block is what keeps this report derivable"
    );

    assert_eq!(
        Attention::from_settings(&resolved.attention)
            .config()
            .volume,
        1.0,
        "the same clamp is what a real caller gets"
    );
}

#[test]
fn default_and_an_empty_documents_resolution_are_indistinguishable() {
    assert_eq!(
        ResolvedTuiConfig::default(),
        resolve("{}"),
        "a field that `resolve` populates but `default` does not is a default that drifts"
    );
}

// ---------------------------------------------------------------------------
// Discovery and the layered merge
// ---------------------------------------------------------------------------

/// Write `text` to `dir/name` and return the path, so a test reads like the file
/// tree it is describing.
fn layer(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("the temp directory is writable");
    path
}

/// Discover on a host that can suspend, for the same reason [`resolve`] does: the
/// keybind rewrite is not what these tests are about.
fn discover(paths: &[PathBuf]) -> Result<ResolvedTuiConfig, TuiConfigError> {
    ResolvedTuiConfig::discover(
        paths,
        ResolveOptions {
            terminal_suspend: true,
        },
    )
}

#[test]
fn every_key_round_trips_from_a_file_on_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = layer(
        dir.path(),
        "tui.json",
        r#"{
          "$schema": "https://opencode.ai/tui.json",
          "keybinds": { "session_compact": "ctrl+alt+k" },
          "leader_timeout": 750,
          "prompt": { "max_height": 12, "max_width": 100 },
          "scroll_speed": 2.5,
          "scroll_acceleration": { "enabled": true },
          "diff_style": "stacked",
          "mouse": false,
          "theme": "tokyonight",
          "attention": { "enabled": true, "volume": 0.75 }
        }"#,
    );

    let resolved = discover(std::slice::from_ref(&path)).expect("the layer resolves");

    assert_eq!(
        resolved.keybinds.get("session_compact"),
        Some(&BindingValue::parse("ctrl+alt+k"))
    );
    assert_eq!(resolved.leader_timeout, Duration::from_millis(750));
    assert_eq!(
        resolved.prompt,
        PromptConfig {
            max_height: NonZeroU16::new(12),
            max_width: Some(MaxWidth::Columns(
                NonZeroU16::new(100).expect("100 is positive")
            )),
        }
    );
    assert_eq!(resolved.scroll_speed, Some(2.5));
    assert_eq!(
        resolved.scroll_acceleration,
        Some(ScrollAcceleration { enabled: true })
    );
    assert_eq!(resolved.diff_style, Some(DiffStyle::Stacked));
    assert!(!resolved.mouse);
    assert_eq!(resolved.theme, "tokyonight");
    assert_eq!(resolved.attention.enabled, Some(true));
    assert_eq!(
        resolved.attention.volume,
        Some(0.75),
        "a key that discovery drops is a key the user cannot configure from a file at all"
    );
}

#[test]
fn a_key_no_layer_mentions_falls_back_to_the_documented_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = layer(dir.path(), "tui.json", r#"{ "theme": "tokyonight" }"#);

    let resolved = discover(std::slice::from_ref(&path)).expect("the layer resolves");

    assert_eq!(resolved.theme, "tokyonight");
    assert_eq!(
        resolved.leader_timeout, DEFAULT_LEADER_TIMEOUT,
        "an unmentioned key takes the same default it would with no file at all"
    );
    assert!(resolved.mouse);
    assert_eq!(resolved.prompt, PromptConfig::default());
    assert_eq!(resolved.attention, AttentionSettings::default());
}

#[test]
fn a_later_layer_wins_key_by_key_without_erasing_the_earlier_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let low = layer(
        dir.path(),
        "global.json",
        r#"{
          "keybinds": { "session_new": "ctrl+n", "app_help": "f1" },
          "leader_timeout": 750,
          "theme": "gruvbox",
          "prompt": { "max_width": 100 },
          "attention": { "enabled": true, "sounds": { "permission": "/low/ping.mp3" } }
        }"#,
    );
    let high = layer(
        dir.path(),
        "project.json",
        r#"{
          "keybinds": { "session_new": "ctrl+t" },
          "theme": "tokyonight",
          "prompt": { "max_height": 12 },
          "attention": { "volume": 0.5, "sounds": { "done": "/high/done.mp3" } }
        }"#,
    );

    let resolved = discover(&[low, high]).expect("both layers resolve");

    assert_eq!(
        resolved.theme, "tokyonight",
        "the later path is the higher-precedence layer"
    );
    assert_eq!(
        resolved.keybinds.get("session_new"),
        Some(&BindingValue::parse("ctrl+t")),
        "a rebound action takes the later spelling"
    );
    assert_eq!(
        resolved.keybinds.get("app_help"),
        Some(&BindingValue::parse("f1")),
        "a binding the later layer is silent about survives; replacing the map would \
         make the nearest file the only file whose keybinds exist"
    );
    assert_eq!(
        resolved.leader_timeout,
        Duration::from_millis(750),
        "a scalar only the earlier layer sets is not reset to its default"
    );
    assert_eq!(
        resolved.prompt,
        PromptConfig {
            max_height: NonZeroU16::new(12),
            max_width: Some(MaxWidth::Columns(
                NonZeroU16::new(100).expect("100 is positive")
            )),
        },
        "nested blocks merge field-wise, so `max_height` alone does not erase `max_width`"
    );
    assert_eq!(resolved.attention.enabled, Some(true));
    assert_eq!(resolved.attention.volume, Some(0.5));
    assert_eq!(
        resolved.attention.sounds.get(&SoundName::Permission),
        Some(&PathBuf::from("/low/ping.mp3")),
        "the sound map unions per slot"
    );
    assert_eq!(
        resolved.attention.sounds.get(&SoundName::Done),
        Some(&PathBuf::from("/high/done.mp3"))
    );
}

#[test]
fn a_missing_layer_is_skipped_rather_than_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let present = layer(dir.path(), "tui.json", r#"{ "theme": "tokyonight" }"#);
    let absent = dir.path().join("nothing-here.json");

    let resolved = discover(&[dir.path().join("also-absent.json"), present, absent])
        .expect("an absent candidate contributes nothing");

    assert_eq!(resolved.theme, "tokyonight");
    assert_eq!(
        discover(&[dir.path().join("absent.json")]).expect("every layer may be absent"),
        ResolvedTuiConfig::default(),
        "a caller offers candidates, so no file at all has to mean the plain defaults"
    );
}

#[test]
fn an_unparsable_layer_is_reported_by_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = layer(dir.path(), "global.json", r#"{ "theme": "gruvbox" }"#);
    let bad = layer(dir.path(), "project.json", "{ not json");

    let error = discover(&[good, bad.clone()]).expect_err("invalid JSON is not a skipped layer");

    let TuiConfigError::ParseFile { path, message } = &error else {
        panic!("a file-scoped failure must carry its path: {error:?}");
    };
    assert_eq!(path, &bad);
    assert!(
        error.to_string().contains(&bad.display().to_string()),
        "the rendered message must name the offending file, or the user opens every \
         candidate looking for the typo: {error}"
    );
    assert!(
        !message.is_empty(),
        "the deserializer's own message names the failing position"
    );
}

#[test]
fn a_layer_that_is_a_directory_is_read_failure_not_absence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let masquerading = dir.path().join("tui.json");
    std::fs::create_dir(&masquerading).expect("the temp directory is writable");

    let error = discover(std::slice::from_ref(&masquerading))
        .expect_err("a path that exists but cannot be read is not an absent layer");

    let TuiConfigError::Read { path, .. } = &error else {
        panic!("an unreadable layer is a read failure: {error:?}");
    };
    assert_eq!(path, &masquerading);
    assert!(
        error
            .to_string()
            .contains(&masquerading.display().to_string()),
        "{error}"
    );
}

#[test]
fn an_out_of_range_value_from_a_file_is_rejected_by_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = layer(dir.path(), "tui.json", r#"{ "scroll_speed": 0.0001 }"#);

    let error = discover(std::slice::from_ref(&path)).expect_err("the value is out of range");

    assert_eq!(
        error,
        TuiConfigError::OutOfRange {
            key: "scroll_speed",
            expected: "at least 0.001",
            found: "0.0001".to_owned(),
        },
        "discovery reuses the existing range vocabulary rather than inventing a second one"
    );
}

#[test]
fn a_shape_error_names_its_own_layer_before_the_merge_can_hide_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bad = layer(dir.path(), "global.json", r#"{ "leader_timeout": 0 }"#);
    let good = layer(dir.path(), "project.json", r#"{ "leader_timeout": 750 }"#);

    let error = discover(&[bad.clone(), good]).expect_err("a zero timeout is rejected at its file");

    let TuiConfigError::ParseFile { path, .. } = &error else {
        panic!("expected the failing layer to be named: {error:?}");
    };
    assert_eq!(
        path, &bad,
        "shape errors are per layer, so the path is knowable; only the range checks \
         in `resolve` run on the merged document, where no single file is to blame"
    );
}

#[test]
fn an_input_undo_from_a_lower_layer_still_blocks_the_suspend_rewrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let low = layer(
        dir.path(),
        "global.json",
        r#"{ "keybinds": { "input_undo": "ctrl+u" } }"#,
    );
    let high = layer(dir.path(), "project.json", r#"{ "theme": "tokyonight" }"#);

    let resolved = ResolvedTuiConfig::discover(
        &[low, high],
        ResolveOptions {
            terminal_suspend: false,
        },
    )
    .expect("both layers resolve");

    assert_eq!(
        resolved.keybinds.get("input_undo"),
        Some(&BindingValue::parse("ctrl+u")),
        "merging has to finish before the rewrite, or a lower layer's `input_undo` \
         reads as absent and gets `ctrl+z` prepended to it"
    );
    assert_eq!(
        resolved.keybinds.get("terminal_suspend"),
        Some(&BindingValue::Disabled),
        "the rest of the rewrite still applies"
    );
}

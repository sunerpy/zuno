//! Theme resolution tests.
//!
//! Every test name starts with `theme_` so the plan's literal filter,
//! `cargo test -p oc-tui theme`, selects all of them rather than reporting
//! `0 passed; N filtered out`.
//!
//! No test touches process-global state: the terminal probe is a fake, the
//! `COLORFGBG` parser is exercised as a pure function rather than through the
//! environment, and nothing installs a panic hook. That is what lets the whole file
//! run concurrently with `app_tests.rs`, which does own a process-global hook.

use std::collections::BTreeMap;
use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::style::Color;

use super::*;
use crate::app::render_offscreen;
use crate::config::TuiConfig;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A probe that answers with whatever the test wants, including nothing.
struct FakePalette(Option<TerminalColors>);

impl TerminalPalette for FakePalette {
    fn query(&self) -> Option<TerminalColors> {
        self.0.clone()
    }
}

/// A terminal that reports a full 16-colour palette over a dark background.
fn dark_terminal() -> TerminalColors {
    TerminalColors {
        default_background: Some(Rgba::opaque(0x0a, 0x0a, 0x0a)),
        default_foreground: Some(Rgba::opaque(0xd0, 0xd0, 0xd0)),
        palette: ANSI_16.iter().copied().map(Some).collect(),
    }
}

/// A definition that sets every required key to one colour, so a test can vary one
/// key without tripping over the other 51.
fn flat_theme(color: &str) -> ThemeJson {
    let body: BTreeMap<String, String> = Palette::REQUIRED_KEYS
        .iter()
        .map(|key| ((*key).to_owned(), color.to_owned()))
        .collect();
    let json = serde_json::json!({ "theme": body });
    ThemeJson::parse(&json.to_string()).expect("a flat theme is valid JSON")
}

/// Serialize a rendered buffer as one line per row, collapsing runs of identical
/// style.
///
/// The point of snapshotting the buffer rather than the palette is that it proves
/// the colours reached cells. Runs keep the diff of a one-colour change to one line.
fn buffer_text(buffer: &Buffer) -> String {
    let describe = |color: Color| -> String {
        match color {
            Color::Reset => String::from("reset"),
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            other => format!("{other:?}"),
        }
    };
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut runs: Vec<String> = Vec::new();
        let mut text = String::new();
        let mut style: Option<(Color, Color, ratatui::style::Modifier)> = None;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let current = (cell.fg, cell.bg, cell.modifier);
            if style != Some(current) {
                if let Some((fg, bg, modifier)) = style {
                    runs.push(format!(
                        "{text:?} fg={} bg={} mod={modifier:?}",
                        describe(fg),
                        describe(bg)
                    ));
                }
                text.clear();
                style = Some(current);
            }
            text.push_str(cell.symbol());
        }
        if let Some((fg, bg, modifier)) = style {
            runs.push(format!(
                "{text:?} fg={} bg={} mod={modifier:?}",
                describe(fg),
                describe(bg)
            ));
        }
        out.push_str(&runs.join(" | "));
        out.push('\n');
    }
    out
}

/// Render a theme's sample view and serialize it, the snapshot subject.
fn theme_sample(registry: &ThemeRegistry, name: &str, mode: Mode) -> String {
    let resolved = registry.resolve(name, mode);
    assert!(
        resolved.issues.is_empty(),
        "built-in theme {name:?} resolved with issues: {:?}",
        resolved.diagnostics()
    );
    let mut view = PaletteSampleView::new(&resolved);
    let height = view.height();
    let buffer = render_offscreen(&mut view, SAMPLE_VIEW_WIDTH, height)
        .expect("the offscreen backend is infallible");
    buffer_text(&buffer)
}

// ---------------------------------------------------------------------------
// The 33 built-in themes
// ---------------------------------------------------------------------------

#[test]
fn theme_builtin_asset_table_holds_every_shipped_theme() {
    let names = builtin_theme_names();
    assert_eq!(
        names.len(),
        BUILTIN_THEME_COUNT,
        "the embedded table must hold exactly {BUILTIN_THEME_COUNT} themes"
    );
    let unique: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(
        unique.len(),
        BUILTIN_THEME_COUNT,
        "theme names must be unique"
    );

    // Floor assertion in the sense WORKTREE.md requires: the asset directory is read
    // at test time, so a file deleted from disk but still listed in the table (or
    // the reverse) fails here rather than passing vacuously.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/themes");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
            } else {
                None
            }
        })
        .collect();
    on_disk.sort();
    assert_eq!(
        on_disk.len(),
        BUILTIN_THEME_COUNT,
        "expected {BUILTIN_THEME_COUNT} JSON assets in {}, found {}",
        dir.display(),
        on_disk.len()
    );
    let mut listed: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    listed.sort();
    assert_eq!(
        listed, on_disk,
        "the embedded table and the asset directory must name the same themes"
    );
}

#[test]
fn theme_registry_parses_every_builtin_theme() {
    let registry = ThemeRegistry::new();
    assert_eq!(
        registry.load_issues(),
        &[] as &[String],
        "every embedded asset must parse"
    );
    assert_eq!(registry.builtin_count(), BUILTIN_THEME_COUNT);
    for name in builtin_theme_names() {
        assert_eq!(registry.layer_of(name), Some(ThemeLayer::Builtin));
    }
}

#[test]
fn theme_every_builtin_theme_sets_every_required_key() {
    let registry = ThemeRegistry::new();
    for name in builtin_theme_names() {
        let definition = registry
            .definition(name)
            .unwrap_or_else(|| panic!("built-in theme {name:?} is missing"));
        let keys: BTreeSet<&str> = definition.keys().into_iter().collect();
        for required in Palette::REQUIRED_KEYS {
            assert!(
                keys.contains(required),
                "built-in theme {name:?} omits required key {required:?}"
            );
        }
    }
}

#[test]
fn theme_resolves_in_both_modes_without_issues() {
    let registry = ThemeRegistry::new();
    for name in builtin_theme_names() {
        for mode in [Mode::Dark, Mode::Light] {
            let resolved = registry.resolve(name, mode);
            assert!(
                resolved.issues.is_empty(),
                "theme {name:?} in {mode:?} reported {:?}",
                resolved.diagnostics()
            );
            assert_eq!(resolved.name, name);
            assert_eq!(resolved.palette.entries().len(), 52);
        }
    }
}

#[test]
fn theme_snapshot_per_builtin_theme() {
    let registry = ThemeRegistry::new();
    let names = builtin_theme_names();
    assert_eq!(names.len(), BUILTIN_THEME_COUNT, "one snapshot per theme");
    for name in names {
        insta::assert_snapshot!(name, theme_sample(&registry, name, Mode::Dark));
    }
}

// ---------------------------------------------------------------------------
// The four layers
// ---------------------------------------------------------------------------

#[test]
fn theme_layers_override_the_layer_below_them() {
    // The ladder, one rung per layer, in the precedence the oracle's own comment
    // states at `packages/tui/src/theme/index.ts:172`:
    //   defaults < plugin installs < custom files < generated system.
    let mut registry = ThemeRegistry::new();

    // Rung 1: only the built-in layer provides `dracula`.
    assert_eq!(registry.layer_of("dracula"), Some(ThemeLayer::Builtin));
    let builtin = registry.resolve("dracula", Mode::Dark).palette.primary;

    // Rung 2: a plugin theme of the same name wins over the built-in.
    let plugin_color = Rgba::opaque(0x11, 0x22, 0x33);
    registry.upsert_theme("dracula", flat_theme("#112233"));
    assert_eq!(registry.layer_of("dracula"), Some(ThemeLayer::Plugin));
    let plugin = registry.resolve("dracula", Mode::Dark).palette.primary;
    assert_eq!(plugin, plugin_color);
    assert_ne!(
        plugin, builtin,
        "the plugin layer must override the built-in"
    );

    // Rung 3: a user's custom file of the same name wins over the plugin.
    let custom_color = Rgba::opaque(0x44, 0x55, 0x66);
    registry.set_custom_themes(BTreeMap::from([(
        String::from("dracula"),
        flat_theme("#445566"),
    )]));
    assert_eq!(registry.layer_of("dracula"), Some(ThemeLayer::Custom));
    let custom = registry.resolve("dracula", Mode::Dark).palette.primary;
    assert_eq!(custom, custom_color);
    assert_ne!(
        custom, plugin,
        "the custom layer must override the plugin layer"
    );

    // Rung 4: the generated system layer wins over a custom theme of that name.
    // The system layer publishes exactly one name (`index.ts:179-182`), so the
    // fourth rung is necessarily tested on `system`.
    let named_system = Rgba::opaque(0x77, 0x88, 0x99);
    registry.set_custom_themes(BTreeMap::from([(
        String::from(SYSTEM_THEME),
        flat_theme("#778899"),
    )]));
    assert_eq!(registry.layer_of(SYSTEM_THEME), Some(ThemeLayer::Custom));
    assert_eq!(
        registry.resolve(SYSTEM_THEME, Mode::Dark).palette.primary,
        named_system
    );
    let outcome =
        registry.refresh_system_theme(&FakePalette(Some(dark_terminal())), None, Mode::Dark);
    assert_eq!(outcome, SystemThemeOutcome::Derived(Mode::Dark));
    assert_eq!(registry.layer_of(SYSTEM_THEME), Some(ThemeLayer::System));
    let system = registry.resolve(SYSTEM_THEME, Mode::Dark).palette.primary;
    assert_ne!(
        system, named_system,
        "the system layer must override the custom layer"
    );
    // Derived from the terminal's cyan (`index.ts:401`), not from any asset.
    assert_eq!(system, ANSI_16[6]);
}

#[test]
fn theme_system_layer_shadows_only_its_own_name() {
    // `index.ts:179-182` merges the system theme under the single key `system`, so
    // it must not affect any other name. Getting this wrong would make every theme
    // silently become the terminal-derived one.
    let mut registry = ThemeRegistry::new();
    let before = registry.resolve("dracula", Mode::Dark).palette;
    registry.refresh_system_theme(&FakePalette(Some(dark_terminal())), None, Mode::Dark);
    assert_eq!(registry.resolve("dracula", Mode::Dark).palette, before);
    assert!(registry.names().contains(&String::from(SYSTEM_THEME)));
}

#[test]
fn theme_plugin_add_refuses_to_shadow_an_existing_name() {
    // `index.ts:220-227`: `addTheme` may add but never replace. Only `upsertTheme`
    // replaces, which is how the plugin layer can shadow a built-in at all.
    let mut registry = ThemeRegistry::new();
    assert!(!registry.add_plugin_theme("dracula", flat_theme("#112233")));
    assert_eq!(registry.layer_of("dracula"), Some(ThemeLayer::Builtin));
    assert!(!registry.add_plugin_theme("", flat_theme("#112233")));

    assert!(registry.add_plugin_theme("brand-new", flat_theme("#112233")));
    assert_eq!(registry.layer_of("brand-new"), Some(ThemeLayer::Plugin));
    assert!(registry.names().contains(&String::from("brand-new")));
    assert!(!registry.add_plugin_theme("brand-new", flat_theme("#445566")));
}

#[test]
fn theme_upsert_writes_to_the_layer_that_already_holds_the_name() {
    let mut registry = ThemeRegistry::new();
    registry.set_custom_themes(BTreeMap::from([(
        String::from("mine"),
        flat_theme("#112233"),
    )]));
    assert!(registry.upsert_theme("mine", flat_theme("#445566")));
    assert_eq!(registry.layer_of("mine"), Some(ThemeLayer::Custom));
    assert!(registry.upsert_theme("theirs", flat_theme("#445566")));
    assert_eq!(registry.layer_of("theirs"), Some(ThemeLayer::Plugin));
}

// ---------------------------------------------------------------------------
// theme: "system" without a terminal
// ---------------------------------------------------------------------------

#[test]
fn theme_system_derives_from_terminal_capabilities_when_available() {
    let mut registry = ThemeRegistry::new();
    let outcome =
        registry.refresh_system_theme(&FakePalette(Some(dark_terminal())), None, Mode::Light);
    // The terminal's own dark background wins over the caller's fallback mode
    // (`src/context/theme.tsx:165`).
    assert_eq!(outcome, SystemThemeOutcome::Derived(Mode::Dark));
    let resolved = registry.resolve(SYSTEM_THEME, Mode::Dark);
    assert!(
        resolved.issues.is_empty(),
        "the derived theme sets every required key: {:?}",
        resolved.diagnostics()
    );
    assert_eq!(resolved.name, SYSTEM_THEME);
    // `index.ts:417`: the background stays transparent so terminal transparency
    // survives.
    assert_eq!(resolved.palette.background.a, 0);
    assert_eq!(
        Color::from(resolved.palette.background),
        Color::Reset,
        "a transparent palette background must render as a reset cell"
    );
    assert!(resolved.palette.has_selected_list_item_text);
}

#[test]
fn theme_system_without_terminal_capabilities_does_not_panic() {
    // The acceptance criterion: under `cargo test` there is no terminal, so the
    // probe answers `None` and `theme: "system"` must still produce a palette.
    let mut registry = ThemeRegistry::new();
    let outcome = registry.refresh_system_theme(&FakePalette(None), None, Mode::Dark);
    assert_eq!(outcome, SystemThemeOutcome::Unavailable);
    assert!(!registry.names().contains(&String::from(SYSTEM_THEME)));

    let resolved = registry.resolve(SYSTEM_THEME, Mode::Dark);
    assert_eq!(resolved.name, DEFAULT_THEME);
    assert_eq!(
        resolved.diagnostics(),
        vec![String::from(
            "theme \"opencode\": no theme named \"system\" in any layer; falling back to the built-in \"opencode\" theme"
        )]
    );
    assert_eq!(
        resolved.palette,
        registry.resolve(DEFAULT_THEME, Mode::Dark).palette
    );
}

#[test]
fn theme_system_refresh_clears_a_stale_derived_layer() {
    let mut registry = ThemeRegistry::new();
    registry.refresh_system_theme(&FakePalette(Some(dark_terminal())), None, Mode::Dark);
    assert_eq!(registry.layer_of(SYSTEM_THEME), Some(ThemeLayer::System));
    assert_eq!(
        registry.refresh_system_theme(&FakePalette(None), None, Mode::Dark),
        SystemThemeOutcome::Unavailable
    );
    assert_eq!(registry.layer_of(SYSTEM_THEME), None);
}

#[test]
fn theme_system_honours_a_locked_mode_and_a_light_terminal() {
    let mut registry = ThemeRegistry::new();
    let light = TerminalColors {
        default_background: Some(Rgba::opaque(0xff, 0xff, 0xff)),
        ..dark_terminal()
    };
    assert_eq!(terminal_mode(&light), Some(Mode::Light));
    assert_eq!(
        registry.refresh_system_theme(&FakePalette(Some(light.clone())), None, Mode::Dark),
        SystemThemeOutcome::Derived(Mode::Light)
    );
    // A user lock beats the terminal's reading (`src/context/theme.tsx:165`).
    assert_eq!(
        registry.refresh_system_theme(&FakePalette(Some(light)), Some(Mode::Dark), Mode::Light),
        SystemThemeOutcome::Derived(Mode::Dark)
    );
}

#[test]
fn theme_system_falls_back_to_the_caller_mode_when_the_terminal_is_silent() {
    let mut registry = ThemeRegistry::new();
    let mute = TerminalColors {
        default_background: None,
        default_foreground: None,
        palette: ANSI_16.iter().copied().map(Some).collect(),
    };
    assert_eq!(terminal_mode(&mute), None);
    assert_eq!(
        registry.refresh_system_theme(&FakePalette(Some(mute)), None, Mode::Light),
        SystemThemeOutcome::Derived(Mode::Light)
    );
}

#[test]
fn theme_system_is_unavailable_when_the_terminal_reports_nothing_usable() {
    let mut registry = ThemeRegistry::new();
    let empty = TerminalColors::default();
    assert!(derive_system_theme(&empty, Mode::Dark).is_none());
    assert_eq!(
        registry.refresh_system_theme(&FakePalette(Some(empty)), None, Mode::Dark),
        SystemThemeOutcome::Unavailable
    );
}

#[test]
fn theme_colorfgbg_is_parsed_without_touching_the_environment() {
    // Mutating `COLORFGBG` would race every other test in the process, so the
    // parser is tested directly and `EnvironmentPalette` is a thin read over it.
    assert_eq!(parse_colorfgbg("15;0"), Some((15, 0)));
    assert_eq!(parse_colorfgbg("15;default;0"), Some((15, 0)));
    assert_eq!(parse_colorfgbg(" 7 ; 0 "), Some((7, 0)));
    assert_eq!(parse_colorfgbg("default;default"), None);
    assert_eq!(parse_colorfgbg(""), None);
    assert_eq!(parse_colorfgbg("1;2;3;4"), None);
    assert_eq!(parse_colorfgbg("999;0"), None, "an ANSI index is a byte");
}

// ---------------------------------------------------------------------------
// Missing keys fall back with a diagnostic
// ---------------------------------------------------------------------------

#[test]
fn theme_missing_key_falls_back_with_a_diagnostic_naming_the_key() {
    let mut theme = flat_theme("#112233");
    theme.theme.remove("primary");
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("gappy", theme);

    let resolved = registry.resolve("gappy", Mode::Dark);
    assert_eq!(
        resolved.issues,
        vec![ThemeIssue::MissingKey { key: "primary" }]
    );
    assert_eq!(
        resolved.diagnostics(),
        vec![String::from(
            "theme \"gappy\": missing color key \"primary\"; falling back to the built-in \"opencode\" theme's value for \"primary\""
        )]
    );
    // The fallback source is the built-in default theme, not the layer below and not
    // a hardcoded colour.
    assert_eq!(
        resolved.palette.primary,
        registry.resolve(DEFAULT_THEME, Mode::Dark).palette.primary
    );
    // Every other key still came from the theme itself.
    assert_eq!(resolved.palette.text, Rgba::opaque(0x11, 0x22, 0x33));
}

#[test]
fn theme_missing_every_key_still_renders() {
    // The pathological case: a `theme` object with nothing in it. 50 diagnostics and
    // a fully populated palette, no panic.
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme(
        "empty",
        ThemeJson::parse("{\"theme\": {}}").expect("an empty theme object is valid"),
    );
    let resolved = registry.resolve("empty", Mode::Dark);
    assert_eq!(resolved.issues.len(), Palette::REQUIRED_KEYS.len());
    assert_eq!(
        resolved.palette,
        registry.resolve(DEFAULT_THEME, Mode::Dark).palette
    );
    let mut view = PaletteSampleView::new(&resolved);
    let height = view.height();
    assert!(render_offscreen(&mut view, SAMPLE_VIEW_WIDTH, height).is_ok());
}

#[test]
fn theme_unresolvable_reference_falls_back_with_a_diagnostic() {
    let mut theme = flat_theme("#112233");
    theme.theme.insert(
        String::from("primary"),
        ColorValue::Scalar(ScalarColor::Reference(String::from("nope"))),
    );
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("dangling", theme);
    let resolved = registry.resolve("dangling", Mode::Dark);
    assert_eq!(
        resolved.issues,
        vec![ThemeIssue::UnknownReference {
            key: "primary",
            reference: String::from("nope"),
        }]
    );
    assert!(resolved.diagnostics()[0].contains("\"nope\""));
    assert!(resolved.diagnostics()[0].contains("\"primary\""));
}

#[test]
fn theme_circular_reference_falls_back_with_a_diagnostic() {
    // The oracle throws here (`index.ts:251`). Throwing would abort the frame, so
    // the cycle becomes a diagnostic that prints the whole chain.
    let mut theme = flat_theme("#112233");
    theme.theme.insert(
        String::from("primary"),
        ColorValue::Scalar(ScalarColor::Reference(String::from("secondary"))),
    );
    theme.theme.insert(
        String::from("secondary"),
        ColorValue::Scalar(ScalarColor::Reference(String::from("primary"))),
    );
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("looped", theme);
    let resolved = registry.resolve("looped", Mode::Dark);
    assert_eq!(
        resolved.issues,
        vec![
            ThemeIssue::CircularReference {
                key: "primary",
                chain: vec![
                    String::from("secondary"),
                    String::from("primary"),
                    String::from("secondary"),
                ],
            },
            ThemeIssue::CircularReference {
                key: "secondary",
                chain: vec![
                    String::from("primary"),
                    String::from("secondary"),
                    String::from("primary"),
                ],
            },
        ]
    );
    assert!(resolved.diagnostics()[0].contains("secondary -> primary -> secondary"));
}

#[test]
fn theme_malformed_hex_falls_back_with_a_diagnostic_quoting_the_value() {
    let theme = flat_theme("#nothex");
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("bogus", theme);
    let resolved = registry.resolve("bogus", Mode::Dark);
    assert_eq!(resolved.issues.len(), Palette::REQUIRED_KEYS.len());
    // Issues arrive in the palette's declaration order, which is the oracle's
    // `Theme` member order (`index.ts:36-92`), so the first one is `primary`.
    assert_eq!(
        resolved.issues[0],
        ThemeIssue::MalformedColor {
            key: "primary",
            value: String::from("#nothex"),
        }
    );
    assert!(resolved.diagnostics()[0].contains("\"#nothex\""));
}

#[test]
fn theme_unknown_name_falls_back_with_a_diagnostic() {
    let registry = ThemeRegistry::new();
    let resolved = registry.resolve("no-such-theme", Mode::Dark);
    assert_eq!(resolved.name, DEFAULT_THEME);
    assert_eq!(
        resolved.issues,
        vec![ThemeIssue::UnknownTheme {
            requested: String::from("no-such-theme"),
        }]
    );
    assert!(resolved.diagnostics()[0].contains("\"no-such-theme\""));
    assert!(!registry.has("no-such-theme"));
    assert!(!registry.has(""));
}

#[test]
fn theme_non_numeric_thinking_opacity_falls_back_with_a_diagnostic() {
    let mut theme = flat_theme("#112233");
    theme.theme.insert(
        String::from("thinkingOpacity"),
        ColorValue::Scalar(ScalarColor::Literal(Rgba::TRANSPARENT)),
    );
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("opaque", theme);
    let resolved = registry.resolve("opaque", Mode::Dark);
    assert_eq!(
        resolved.issues,
        vec![ThemeIssue::NotANumber {
            key: "thinkingOpacity"
        }]
    );
    assert!((resolved.palette.thinking_opacity - DEFAULT_THINKING_OPACITY).abs() < f64::EPSILON);
}

// ---------------------------------------------------------------------------
// Optional keys and their in-theme fallbacks
// ---------------------------------------------------------------------------

#[test]
fn theme_optional_keys_fall_back_inside_the_same_theme() {
    // `index.ts:274-289`: these two are documented as optional, so their absence is
    // not a diagnostic — they resolve from another key of the same theme.
    let registry = ThemeRegistry::new();
    for name in builtin_theme_names() {
        let definition = registry.definition(name).expect("built-in");
        let keys: BTreeSet<&str> = definition.keys().into_iter().collect();
        let resolved = registry.resolve(name, Mode::Dark);
        assert!(resolved.issues.is_empty());
        if !keys.contains("selectedListItemText") {
            assert_eq!(
                resolved.palette.selected_list_item_text, resolved.palette.background,
                "{name:?} must fall back to its own background"
            );
            assert!(!resolved.palette.has_selected_list_item_text);
        } else {
            assert!(resolved.palette.has_selected_list_item_text);
        }
        if keys.contains("backgroundMenu") {
            continue;
        }
        assert_eq!(
            resolved.palette.background_menu, resolved.palette.background_element,
            "{name:?} must fall back to its own backgroundElement"
        );
    }
}

#[test]
fn theme_selected_foreground_follows_the_oracle_branches() {
    let registry = ThemeRegistry::new();
    // An explicit key wins outright (`index.ts:98-100`).
    let mut explicit = registry.resolve(DEFAULT_THEME, Mode::Dark).palette;
    explicit.has_selected_list_item_text = true;
    explicit.selected_list_item_text = Rgba::opaque(1, 2, 3);
    assert_eq!(selected_foreground(&explicit, None), Rgba::opaque(1, 2, 3));

    // A transparent background carries no contrast, so contrast comes from the row
    // colour (`index.ts:102-107`).
    let mut transparent = explicit.clone();
    transparent.has_selected_list_item_text = false;
    transparent.background = Rgba::TRANSPARENT;
    assert_eq!(
        selected_foreground(&transparent, Some(Rgba::opaque(255, 255, 255))),
        Rgba::opaque(0, 0, 0)
    );
    assert_eq!(
        selected_foreground(&transparent, Some(Rgba::opaque(0, 0, 0))),
        Rgba::opaque(255, 255, 255)
    );

    // Otherwise the background itself (`index.ts:109`).
    let mut opaque = transparent.clone();
    opaque.background = Rgba::opaque(9, 9, 9);
    assert_eq!(selected_foreground(&opaque, None), Rgba::opaque(9, 9, 9));
}

// ---------------------------------------------------------------------------
// Colour primitives
// ---------------------------------------------------------------------------

#[test]
fn theme_hex_parsing_accepts_three_six_and_eight_digits() {
    assert_eq!(Rgba::from_hex("#abc"), Some(Rgba::opaque(0xaa, 0xbb, 0xcc)));
    assert_eq!(
        Rgba::from_hex("#0a1b2c"),
        Some(Rgba::opaque(0x0a, 0x1b, 0x2c))
    );
    assert_eq!(
        Rgba::from_hex("#0a1b2c80"),
        Some(Rgba {
            r: 0x0a,
            g: 0x1b,
            b: 0x2c,
            a: 0x80
        })
    );
    for bad in ["abc", "#ab", "#abcd", "#abcdz", "#", "#1234567"] {
        assert_eq!(Rgba::from_hex(bad), None, "{bad:?} must not parse");
    }
}

#[test]
fn theme_transparent_and_none_are_both_fully_transparent() {
    // `index.ts:239`.
    let json = "{\"theme\": {\"background\": \"transparent\", \"backgroundPanel\": \"none\"}}";
    let theme = ThemeJson::parse(json).expect("valid");
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("clear", theme);
    let resolved = registry.resolve("clear", Mode::Dark);
    assert_eq!(resolved.palette.background, Rgba::TRANSPARENT);
    assert_eq!(resolved.palette.background_panel, Rgba::TRANSPARENT);
}

#[test]
fn theme_ansi_indices_map_to_the_oracle_table() {
    // `index.ts:301-344`.
    assert_eq!(ansi_to_rgba(0), Rgba::opaque(0, 0, 0));
    assert_eq!(ansi_to_rgba(7), Rgba::opaque(0xc0, 0xc0, 0xc0));
    assert_eq!(ansi_to_rgba(15), Rgba::opaque(0xff, 0xff, 0xff));
    assert_eq!(ansi_to_rgba(16), Rgba::opaque(0, 0, 0));
    assert_eq!(ansi_to_rgba(231), Rgba::opaque(255, 255, 255));
    assert_eq!(ansi_to_rgba(232), Rgba::opaque(8, 8, 8));
    assert_eq!(ansi_to_rgba(255), Rgba::opaque(238, 238, 238));
    assert_eq!(ansi_to_rgba(256), Rgba::opaque(0, 0, 0));
    assert_eq!(ansi_to_rgba(-1), Rgba::opaque(0, 0, 0));
}

#[test]
fn theme_numeric_color_values_resolve_as_ansi_indices() {
    let json = "{\"theme\": {\"primary\": 6, \"secondary\": {\"dark\": 5, \"light\": 4}}}";
    let theme = ThemeJson::parse(json).expect("valid");
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("ansi", theme);
    assert_eq!(
        registry.resolve("ansi", Mode::Dark).palette.primary,
        ANSI_16[6]
    );
    assert_eq!(
        registry.resolve("ansi", Mode::Dark).palette.secondary,
        ANSI_16[5]
    );
    assert_eq!(
        registry.resolve("ansi", Mode::Light).palette.secondary,
        ANSI_16[4]
    );
}

#[test]
fn theme_variants_and_defs_resolve_per_mode() {
    let json = r##"{
      "defs": { "ink": "#101010", "paper": "#f0f0f0", "alias": "ink" },
      "theme": {
        "primary": { "dark": "paper", "light": "ink" },
        "secondary": "alias",
        "accent": "primary"
      }
    }"##;
    let theme = ThemeJson::parse(json).expect("valid");
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("refs", theme);
    let dark = registry.resolve("refs", Mode::Dark).palette;
    assert_eq!(dark.primary, Rgba::opaque(0xf0, 0xf0, 0xf0));
    // A def may itself be a reference (`index.ts:254`).
    assert_eq!(dark.secondary, Rgba::opaque(0x10, 0x10, 0x10));
    // A theme key may reference another theme key, variant and all.
    assert_eq!(dark.accent, dark.primary);
    let light = registry.resolve("refs", Mode::Light).palette;
    assert_eq!(light.primary, Rgba::opaque(0x10, 0x10, 0x10));
}

#[test]
fn theme_defs_win_over_a_same_named_theme_key() {
    // `index.ts:254` looks in `defs` first.
    let json = r##"{
      "defs": { "primary": "#010203" },
      "theme": { "primary": "#040506", "secondary": "primary" }
    }"##;
    let theme = ThemeJson::parse(json).expect("valid");
    let mut registry = ThemeRegistry::new();
    registry.upsert_theme("shadow", theme);
    let palette = registry.resolve("shadow", Mode::Dark).palette;
    assert_eq!(palette.primary, Rgba::opaque(0x04, 0x05, 0x06));
    assert_eq!(palette.secondary, Rgba::opaque(0x01, 0x02, 0x03));
}

#[test]
fn theme_json_without_a_theme_object_is_rejected() {
    // The oracle's structural check (`index.ts:194-198`) is what keeps an arbitrary
    // JSON file from being installed as a theme.
    for bad in ["{}", "[]", "\"nope\"", "{\"theme\": []}", "{\"theme\": 1}"] {
        assert!(ThemeJson::parse(bad).is_err(), "{bad:?} must be rejected");
    }
    assert!(ThemeJson::parse("{\"theme\": {}, \"$schema\": \"x\"}").is_ok());
}

#[test]
fn theme_tint_and_gray_scale_match_the_oracle_arithmetic() {
    // `index.ts:346-351`: a zero alpha is the base, a one alpha is the overlay.
    let black = Rgba::opaque(0, 0, 0);
    let white = Rgba::opaque(255, 255, 255);
    assert_eq!(tint(black, white, 0.0), black);
    assert_eq!(tint(black, white, 1.0), white);
    assert_eq!(tint(black, white, 0.5), Rgba::opaque(128, 128, 128));

    // `index.ts:489-494`: a near-black background takes the flat ramp.
    let grays = gray_scale(black, true);
    assert_eq!(grays[0], Rgba::opaque(8, 8, 8));
    assert_eq!(grays[11], Rgba::opaque(102, 102, 102));
    // `index.ts:536-538`.
    assert_eq!(muted_text(black, true), Rgba::opaque(180, 180, 180));
    // `index.ts:544-546`.
    assert_eq!(muted_text(white, false), Rgba::opaque(75, 75, 75));
}

// ---------------------------------------------------------------------------
// The config key
// ---------------------------------------------------------------------------

#[test]
fn theme_config_key_selects_the_palette_with_no_code_change() {
    // The happy-path QA scenario: only the config value changes.
    let registry = ThemeRegistry::new();
    let mut seen = BTreeSet::new();
    for name in ["opencode", "dracula", "nord", "github"] {
        let config: TuiConfig =
            serde_json::from_str(&format!("{{\"theme\": \"{name}\"}}")).expect("valid config");
        assert_eq!(config.theme(), Some(name));
        let resolved = registry.resolve_configured(config.theme(), Mode::Dark);
        assert_eq!(resolved.name, name);
        assert!(resolved.issues.is_empty());
        assert!(
            seen.insert(resolved.palette.primary),
            "{name:?} must not share a primary with an earlier theme"
        );
    }
}

#[test]
fn theme_config_key_absent_selects_the_default_theme() {
    let registry = ThemeRegistry::new();
    let config: TuiConfig = serde_json::from_str("{}").expect("an empty config is valid");
    assert_eq!(config.theme(), None);
    let resolved = registry.resolve_configured(config.theme(), Mode::Dark);
    assert_eq!(resolved.name, DEFAULT_THEME);
    assert!(resolved.issues.is_empty());
}

#[test]
fn theme_config_key_accepts_the_system_value() {
    let mut registry = ThemeRegistry::new();
    let config: TuiConfig = serde_json::from_str("{\"theme\": \"system\"}").expect("valid config");
    assert_eq!(config.theme(), Some(SYSTEM_THEME));

    // Without a terminal: falls back, with a diagnostic, no panic.
    let unavailable = registry.resolve_configured(config.theme(), Mode::Dark);
    assert_eq!(unavailable.name, DEFAULT_THEME);
    assert_eq!(unavailable.issues.len(), 1);

    // With one: the derived palette.
    registry.refresh_system_theme(&FakePalette(Some(dark_terminal())), None, Mode::Dark);
    let derived = registry.resolve_configured(config.theme(), Mode::Dark);
    assert_eq!(derived.name, SYSTEM_THEME);
    assert!(derived.issues.is_empty());
    assert_ne!(derived.palette, unavailable.palette);
}

#[test]
fn theme_config_round_trips_through_serde() {
    let config = TuiConfig {
        theme: Some(String::from("nord")),
    };
    let json = serde_json::to_string(&config).expect("serializable");
    assert_eq!(json, "{\"theme\":\"nord\"}");
    assert_eq!(
        serde_json::from_str::<TuiConfig>(&json).expect("round trip"),
        config
    );
    // An omitted key must not serialize, so a config file stays minimal.
    assert_eq!(
        serde_json::to_string(&TuiConfig::default()).expect("serializable"),
        "{}"
    );
}

// ---------------------------------------------------------------------------
// No view hardcodes a colour
// ---------------------------------------------------------------------------

#[test]
fn theme_no_view_hardcodes_a_color() {
    // The acceptance requirement stated as a guard: only this module may name a
    // colour. Every other source file in the crate must reach colours through the
    // palette. The floor assertion is mandatory per `.omo/WORKTREE.md` — a scan
    // that finds no files would otherwise pass vacuously.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut scanned = 0usize;
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|error| {
        panic!("cannot read {}: {error}", dir.display());
    }) {
        let path = entry.expect("readable entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        if name == "theme.rs" || name == "theme_tests.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("readable source");
        scanned += 1;
        for (number, line) in source.lines().enumerate() {
            if line.contains("Color::Rgb")
                || line.contains("Color::Indexed")
                || line.contains("Rgba::opaque")
            {
                offenders.push(format!("{name}:{}", number + 1));
            }
        }
    }
    assert!(
        scanned >= 3,
        "scanned only {scanned} source files under {}; the scan is looking in the wrong place and would pass vacuously",
        dir.display()
    );
    assert!(
        offenders.is_empty(),
        "these files name a colour instead of taking it from the palette: {offenders:?}"
    );
}

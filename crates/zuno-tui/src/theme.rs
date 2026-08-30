//! Theme data, four-layer resolution, and the palette every view paints with.
//!
//! Ported from the oracle's `packages/tui/src/theme/index.ts`. Three properties of
//! that module are load-bearing and are reproduced here rather than reinvented:
//!
//! 1. **Themes are data, not code.** The 33 built-in definitions are the oracle's
//!    own JSON assets, embedded verbatim with `include_str!`. Adding a theme is
//!    adding a file; changing a colour is changing one line of JSON. Nothing in
//!    this crate spells a colour for a view.
//! 2. **Four layers with a fixed precedence.** `index.ts:171-183` merges
//!    `DEFAULT_THEMES < pluginThemes < customThemes < system`, and its own comment
//!    (`index.ts:172`) states that order. Note that the *plan's* prose orders user
//!    custom before plugin-provided; the source does the opposite, and the source
//!    is what [`ThemeRegistry::definition`] implements. The system layer is special:
//!    `index.ts:179-182` publishes it under the single name `system`, so it can only
//!    ever shadow that name.
//! 3. **Resolution is a graph walk, not a lookup.** A colour may be a hex literal,
//!    a reference into `defs` or into another theme key, a `{dark, light}` variant,
//!    an ANSI index, or the words `transparent`/`none` (`index.ts:236-264`).
//!
//! # Why a missing key is a diagnostic and not an error
//!
//! The oracle throws on an unresolvable reference (`index.ts:256`) and on a cycle
//! (`index.ts:251`). Throwing is survivable in a JS render tree that can retry with
//! `opencode`; here it would abort the frame. So resolution is total: every failure
//! yields the corresponding colour from the built-in `zuno` theme plus a
//! [`ThemeIssue`] naming the key. A hand-written theme with one typo renders, and
//! says which key was wrong, instead of taking the TUI down.
//!
//! # Why the terminal probe is a trait
//!
//! `theme: "system"` derives its palette from the terminal's own colours
//! (`index.ts:360-469`, driven by `src/context/theme.tsx:152-178`). Under `cargo
//! test` there is no terminal, so the probe is [`TerminalPalette`] — the same
//! trait-plus-fake shape `app::TerminalLifecycle` uses to make lifecycle tests
//! TTY-free. [`HostTerminalPalette`] queries OSC 10/11 before the TUI owns the input
//! stream, then falls back to [`EnvironmentPalette`]'s `COLORFGBG` convention.
//! When neither is available, the built-in `system` theme supplies a readable
//! neutral surface hierarchy.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use serde::Deserialize;
use serde::de::{self, Deserializer, Unexpected, Visitor};

use crate::app::{Component, EventResult};

/// The theme resolved when configuration names nothing, or names something absent.
///
/// Every failure path resolves through this built-in palette.
pub const DEFAULT_THEME: &str = "zuno";

/// The one name the terminal-derived layer can occupy (`index.ts:181`).
///
/// This name exists on **two** tiers, and which one answers depends on whether the
/// terminal told us anything:
///
/// 1. **Derived**, when a probe answers: [`derive_system_theme`] computes a grey
///    scale from the terminal's *actual* background and reads its *actual* palette
///    entries. This is the oracle's own behaviour and is the better tier.
/// 2. **Built-in asset**, otherwise: `assets/themes/system.json`, which preserves
///    the terminal's default foreground and background while supplying fixed ANSI
///    accents and borders.
///
/// Tier 1 still shadows tier 2, because [`ThemeRegistry::definition`] checks the
/// system layer first. Nothing about the derived path changed to add the asset.
pub const SYSTEM_THEME: &str = "system";

/// Default opacity applied to "thinking" text when a theme does not set one
/// (`index.ts:292`).
pub const DEFAULT_THINKING_OPACITY: f64 = 0.6;

// ---------------------------------------------------------------------------
// Colour primitives
// ---------------------------------------------------------------------------

/// An eight-bit-per-channel colour with an alpha channel.
///
/// Alpha is retained rather than flattened because two behaviours depend on it: a
/// fully transparent background means "let the terminal's own background show
/// through", and [`selected_foreground`] branches on exactly that
/// (`index.ts:100-107`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rgba {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel; `0` means the terminal background shows through.
    pub a: u8,
}

impl Rgba {
    /// Fully transparent, carrying no colour of its own.
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// An opaque colour.
    #[must_use]
    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// The same colour with its alpha replaced.
    #[must_use]
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// Parse `#rgb`, `#rrggbb`, or `#rrggbbaa`.
    ///
    /// Returns `None` for anything else; callers turn that into a diagnostic rather
    /// than a panic.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let digits = text.strip_prefix('#')?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let nibble = |index: usize| -> Option<u8> {
            let byte = digits.as_bytes().get(index)?;
            char::from(*byte)
                .to_digit(16)
                .and_then(|v| u8::try_from(v).ok())
        };
        let pair = |index: usize| -> Option<u8> {
            let high = nibble(index)?;
            let low = nibble(index + 1)?;
            Some((high << 4) | low)
        };
        match digits.len() {
            3 => {
                let expand = |index: usize| nibble(index).map(|v| (v << 4) | v);
                Some(Self::opaque(expand(0)?, expand(1)?, expand(2)?))
            }
            6 => Some(Self::opaque(pair(0)?, pair(2)?, pair(4)?)),
            8 => Some(Self {
                r: pair(0)?,
                g: pair(2)?,
                b: pair(4)?,
                a: pair(6)?,
            }),
            _ => None,
        }
    }

    /// Perceptual luminance on a `0.0..=1.0` scale.
    ///
    /// The coefficients are the oracle's (`index.ts:104`, `:357`, `:479`).
    #[must_use]
    pub fn luminance(self) -> f64 {
        0.299 * f64::from(self.r) + 0.587 * f64::from(self.g) + 0.114 * f64::from(self.b)
    }

    /// `#rrggbb`, or `#rrggbbaa` when not fully opaque.
    #[must_use]
    pub fn to_hex(self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

impl From<Rgba> for ratatui::style::Color {
    /// A fully transparent palette colour becomes [`ratatui::style::Color::Reset`],
    /// which is how a terminal cell says "keep whatever the emulator uses".
    fn from(value: Rgba) -> Self {
        if value.a == 0 {
            Self::Reset
        } else {
            Self::Rgb(value.r, value.g, value.b)
        }
    }
}

/// The 16 standard ANSI colours, used when a terminal reports no palette entry.
///
/// Verbatim from `index.ts:304-321`.
const ANSI_16: [Rgba; 16] = [
    Rgba::opaque(0x00, 0x00, 0x00),
    Rgba::opaque(0x80, 0x00, 0x00),
    Rgba::opaque(0x00, 0x80, 0x00),
    Rgba::opaque(0x80, 0x80, 0x00),
    Rgba::opaque(0x00, 0x00, 0x80),
    Rgba::opaque(0x80, 0x00, 0x80),
    Rgba::opaque(0x00, 0x80, 0x80),
    Rgba::opaque(0xc0, 0xc0, 0xc0),
    Rgba::opaque(0x80, 0x80, 0x80),
    Rgba::opaque(0xff, 0x00, 0x00),
    Rgba::opaque(0x00, 0xff, 0x00),
    Rgba::opaque(0xff, 0xff, 0x00),
    Rgba::opaque(0x00, 0x00, 0xff),
    Rgba::opaque(0xff, 0x00, 0xff),
    Rgba::opaque(0x00, 0xff, 0xff),
    Rgba::opaque(0xff, 0xff, 0xff),
];

/// Map an xterm-256 index onto a colour (`index.ts:301-344`).
///
/// Out-of-range indices become black, matching the oracle's final fallback.
#[must_use]
pub fn ansi_to_rgba(code: i64) -> Rgba {
    let Ok(code) = usize::try_from(code) else {
        return ANSI_16[0];
    };
    if let Some(color) = ANSI_16.get(code) {
        return *color;
    }
    if code < 232 {
        let index = code - 16;
        let level = |x: usize| -> u8 {
            let value = if x == 0 { 0 } else { x * 40 + 55 };
            u8::try_from(value).unwrap_or(u8::MAX)
        };
        return Rgba::opaque(level(index / 36), level((index / 6) % 6), level(index % 6));
    }
    if code < 256 {
        let gray = u8::try_from((code - 232) * 10 + 8).unwrap_or(u8::MAX);
        return Rgba::opaque(gray, gray, gray);
    }
    ANSI_16[0]
}

/// Blend `overlay` into `base` by `alpha` (`index.ts:346-351`).
#[must_use]
pub fn tint(base: Rgba, overlay: Rgba, alpha: f64) -> Rgba {
    let mix = |base: u8, overlay: u8| -> u8 {
        let base = f64::from(base);
        let value = base + (f64::from(overlay) - base) * alpha;
        // The oracle rounds after scaling back to 0..255; clamping keeps a caller
        // supplied alpha outside 0..1 from wrapping.
        value.round().clamp(0.0, 255.0) as u8
    };
    Rgba::opaque(
        mix(base.r, overlay.r),
        mix(base.g, overlay.g),
        mix(base.b, overlay.b),
    )
}

/// Whether a theme is being resolved for a dark or a light terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    /// Light-on-dark.
    #[default]
    Dark,
    /// Dark-on-light.
    Light,
}

// ---------------------------------------------------------------------------
// Theme JSON
// ---------------------------------------------------------------------------

/// One colour expression before resolution.
#[derive(Debug, Clone, PartialEq)]
enum ScalarColor {
    /// A hex literal, or `transparent`/`none`.
    Literal(Rgba),
    /// A name to look up in `defs`, then in the theme's own keys.
    Reference(String),
    /// A bare number: an ANSI index as a colour, an opacity as a scalar.
    Number(f64),
    /// A `#`-prefixed string that is not valid hex. Kept so the diagnostic can
    /// quote it instead of silently degrading to a reference lookup.
    Malformed(String),
}

impl<'de> Deserialize<'de> for ScalarColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ScalarVisitor;

        impl Visitor<'_> for ScalarVisitor {
            type Value = ScalarColor;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a hex colour, a colour reference, or a number")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                // `index.ts:239` treats both spellings as fully transparent.
                if value == "transparent" || value == "none" {
                    return Ok(ScalarColor::Literal(Rgba::TRANSPARENT));
                }
                if value.starts_with('#') {
                    return Ok(Rgba::from_hex(value).map_or_else(
                        || ScalarColor::Malformed(value.to_owned()),
                        ScalarColor::Literal,
                    ));
                }
                Ok(ScalarColor::Reference(value.to_owned()))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(ScalarColor::Number(value))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(ScalarColor::Number(value as f64))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(ScalarColor::Number(value as f64))
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Err(E::invalid_type(Unexpected::Bool(value), &self))
            }
        }

        deserializer.deserialize_any(ScalarVisitor)
    }
}

/// A colour expression, which may differ between light and dark mode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
enum ColorValue {
    /// One expression used in both modes.
    Scalar(ScalarColor),
    /// A per-mode pair (`index.ts:115-118`).
    Variant {
        /// The expression used in [`Mode::Dark`].
        dark: ScalarColor,
        /// The expression used in [`Mode::Light`].
        light: ScalarColor,
    },
}

/// A theme definition as it appears on disk.
///
/// `$schema` and any other unknown member is ignored, matching the oracle's
/// structural `isTheme` check (`index.ts:194-198`): a theme is anything with a
/// `theme` object.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ThemeJson {
    /// Named intermediates referenced by `theme` entries (`index.ts:121`).
    #[serde(default)]
    defs: BTreeMap<String, ColorValue>,
    /// The colour keys themselves.
    theme: BTreeMap<String, ColorValue>,
}

impl ThemeJson {
    /// Parse a theme definition from JSON.
    ///
    /// # Errors
    ///
    /// Returns the serde message when the document is not an object with a `theme`
    /// object in it.
    pub fn parse(source: &str) -> Result<Self, String> {
        serde_json::from_str(source).map_err(|error| error.to_string())
    }

    /// Build a definition from already-resolved colours, as the terminal-derived
    /// layer does.
    #[must_use]
    fn from_literals(entries: Vec<(&'static str, Rgba)>) -> Self {
        Self {
            defs: BTreeMap::new(),
            theme: entries
                .into_iter()
                .map(|(key, color)| {
                    (
                        key.to_owned(),
                        ColorValue::Scalar(ScalarColor::Literal(color)),
                    )
                })
                .collect(),
        }
    }

    /// The colour keys this definition sets, in sorted order.
    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        self.theme.keys().map(String::as_str).collect()
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// One recoverable problem found while resolving a theme.
///
/// Every variant names the key or theme at fault, because the diagnostic's whole
/// job is to point at the line of JSON that needs editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeIssue {
    /// Configuration named a theme no layer provides.
    UnknownTheme {
        /// The name that was requested.
        requested: String,
    },
    /// A required colour key is absent from the definition.
    MissingKey {
        /// The absent key.
        key: &'static str,
    },
    /// A reference resolved to no `defs` entry and no theme key.
    UnknownReference {
        /// The key whose value could not be resolved.
        key: &'static str,
        /// The reference that was not found.
        reference: String,
    },
    /// A reference chain returned to a name it already visited.
    CircularReference {
        /// The key whose value could not be resolved.
        key: &'static str,
        /// The cycle, in visit order.
        chain: Vec<String>,
    },
    /// A `#`-prefixed value that is not valid hex.
    MalformedColor {
        /// The key whose value could not be parsed.
        key: &'static str,
        /// The offending literal.
        value: String,
    },
    /// A key expected to hold a number held something else.
    NotANumber {
        /// The key that was not a number.
        key: &'static str,
    },
}

impl std::fmt::Display for ThemeIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTheme { requested } => write!(
                f,
                "no theme named {requested:?} in any layer; falling back to the built-in {DEFAULT_THEME:?} theme"
            ),
            Self::MissingKey { key } => write!(
                f,
                "missing color key {key:?}; falling back to the built-in {DEFAULT_THEME:?} theme's value for {key:?}"
            ),
            Self::UnknownReference { key, reference } => write!(
                f,
                "color key {key:?} references {reference:?}, which is in neither defs nor theme; falling back to the built-in {DEFAULT_THEME:?} theme's value for {key:?}"
            ),
            Self::CircularReference { key, chain } => write!(
                f,
                "color key {key:?} has a circular reference {}; falling back to the built-in {DEFAULT_THEME:?} theme's value for {key:?}",
                chain.join(" -> ")
            ),
            Self::MalformedColor { key, value } => write!(
                f,
                "color key {key:?} is not a valid hex color: {value:?}; falling back to the built-in {DEFAULT_THEME:?} theme's value for {key:?}"
            ),
            Self::NotANumber { key } => write!(
                f,
                "color key {key:?} must be a number; falling back to its default"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// The palette
// ---------------------------------------------------------------------------

/// Declare the palette once, and derive from it the struct, the JSON key names, and
/// the resolution loop.
///
/// A single table is the point: a field the resolver never fills, or a JSON key no
/// field consumes, is not expressible.
macro_rules! declare_palette {
    (
        required { $($rf:ident => $rk:literal),+ $(,)? }
        optional { $($of:ident => $ok:literal , fallback $ofb:ident),+ $(,)? }
    ) => {
        /// Every colour a view is allowed to paint with, fully resolved.
        ///
        /// Field names are the oracle's `Theme` members (`index.ts:36-92`) in snake
        /// case; [`Palette::entries`] pairs each with the JSON key it came from.
        #[derive(Debug, Clone, PartialEq)]
        #[allow(missing_docs, reason = "each field is the identically named theme JSON key")]
        pub struct Palette {
            $(pub $rf: Rgba,)+
            $(pub $of: Rgba,)+
            /// Opacity applied to "thinking" text (`index.ts:292`).
            pub thinking_opacity: f64,
            /// Whether the definition set `selectedListItemText` explicitly, which
            /// changes how [`selected_foreground`] behaves (`index.ts:98-100`).
            pub has_selected_list_item_text: bool,
        }

        impl Palette {
            /// The colour keys every definition must set.
            pub const REQUIRED_KEYS: &'static [&'static str] = &[$($rk),+];

            /// The colour keys a definition may omit, each with a documented
            /// fallback inside the same theme.
            pub const OPTIONAL_KEYS: &'static [&'static str] = &[$($ok),+];

            /// Every colour, paired with its JSON key, in declaration order.
            #[must_use]
            pub fn entries(&self) -> Vec<(&'static str, Rgba)> {
                vec![$(($rk, self.$rf),)+ $(($ok, self.$of),)+]
            }

            /// A palette with one colour everywhere, the seed for [`last_resort`].
            const fn uniform(color: Rgba) -> Self {
                Self {
                    $($rf: color,)+
                    $($of: color,)+
                    thinking_opacity: DEFAULT_THINKING_OPACITY,
                    has_selected_list_item_text: false,
                }
            }
        }

        fn resolve_palette(theme: &ThemeJson, mode: Mode, fallback: &Palette) -> (Palette, Vec<ThemeIssue>) {
            let mut issues = Vec::new();
            let mut resolved = Palette {
                $($rf: resolve_key(theme, mode, $rk, fallback.$rf, &mut issues),)+
                // Optional keys are filled below, once the value they fall back to
                // has itself been resolved.
                $($of: fallback.$of,)+
                thinking_opacity: DEFAULT_THINKING_OPACITY,
                has_selected_list_item_text: false,
            };
            $(
                if theme.theme.contains_key($ok) {
                    resolved.$of = resolve_key(theme, mode, $ok, fallback.$of, &mut issues);
                } else {
                    resolved.$of = resolved.$ofb;
                }
            )+
            resolved.has_selected_list_item_text = theme.theme.contains_key("selectedListItemText");
            resolved.thinking_opacity = thinking_opacity(theme, &mut issues);
            (resolved, issues)
        }
    };
}

declare_palette! {
    required {
        primary => "primary",
        secondary => "secondary",
        accent => "accent",
        error => "error",
        warning => "warning",
        success => "success",
        info => "info",
        text => "text",
        text_muted => "textMuted",
        background => "background",
        background_panel => "backgroundPanel",
        background_element => "backgroundElement",
        border => "border",
        border_active => "borderActive",
        border_subtle => "borderSubtle",
        diff_added => "diffAdded",
        diff_removed => "diffRemoved",
        diff_context => "diffContext",
        diff_hunk_header => "diffHunkHeader",
        diff_highlight_added => "diffHighlightAdded",
        diff_highlight_removed => "diffHighlightRemoved",
        diff_added_bg => "diffAddedBg",
        diff_removed_bg => "diffRemovedBg",
        diff_context_bg => "diffContextBg",
        diff_line_number => "diffLineNumber",
        diff_added_line_number_bg => "diffAddedLineNumberBg",
        diff_removed_line_number_bg => "diffRemovedLineNumberBg",
        markdown_text => "markdownText",
        markdown_heading => "markdownHeading",
        markdown_link => "markdownLink",
        markdown_link_text => "markdownLinkText",
        markdown_code => "markdownCode",
        markdown_block_quote => "markdownBlockQuote",
        markdown_emph => "markdownEmph",
        markdown_strong => "markdownStrong",
        markdown_horizontal_rule => "markdownHorizontalRule",
        markdown_list_item => "markdownListItem",
        markdown_list_enumeration => "markdownListEnumeration",
        markdown_image => "markdownImage",
        markdown_image_text => "markdownImageText",
        markdown_code_block => "markdownCodeBlock",
        syntax_comment => "syntaxComment",
        syntax_keyword => "syntaxKeyword",
        syntax_function => "syntaxFunction",
        syntax_variable => "syntaxVariable",
        syntax_string => "syntaxString",
        syntax_number => "syntaxNumber",
        syntax_type => "syntaxType",
        syntax_operator => "syntaxOperator",
        syntax_punctuation => "syntaxPunctuation",
    }
    optional {
        // `index.ts:274-282`: absent means "use the background", which is what every
        // theme predating the key relied on.
        selected_list_item_text => "selectedListItemText", fallback background,
        // `index.ts:284-289`.
        background_menu => "backgroundMenu", fallback background_element,
    }
}

/// Read `thinkingOpacity`, which shares the `theme` object with the colours but is
/// a scalar (`index.ts:291-292`).
fn thinking_opacity(theme: &ThemeJson, issues: &mut Vec<ThemeIssue>) -> f64 {
    match theme.theme.get("thinkingOpacity") {
        None => DEFAULT_THINKING_OPACITY,
        Some(ColorValue::Scalar(ScalarColor::Number(value))) => *value,
        Some(_) => {
            issues.push(ThemeIssue::NotANumber {
                key: "thinkingOpacity",
            });
            DEFAULT_THINKING_OPACITY
        }
    }
}

/// Resolve one key, substituting `fallback` and recording an issue on any failure.
fn resolve_key(
    theme: &ThemeJson,
    mode: Mode,
    key: &'static str,
    fallback: Rgba,
    issues: &mut Vec<ThemeIssue>,
) -> Rgba {
    let Some(value) = theme.theme.get(key) else {
        issues.push(ThemeIssue::MissingKey { key });
        return fallback;
    };
    match resolve_color(theme, mode, value, &mut Vec::new()) {
        Ok(color) => color,
        Err(error) => {
            issues.push(error.into_issue(key));
            fallback
        }
    }
}

/// A resolution failure, before it is attributed to a key.
enum ResolveError {
    UnknownReference(String),
    Circular(Vec<String>),
    Malformed(String),
}

impl ResolveError {
    fn into_issue(self, key: &'static str) -> ThemeIssue {
        match self {
            Self::UnknownReference(reference) => ThemeIssue::UnknownReference { key, reference },
            Self::Circular(chain) => ThemeIssue::CircularReference { key, chain },
            Self::Malformed(value) => ThemeIssue::MalformedColor { key, value },
        }
    }
}

/// Walk a colour expression to a concrete colour (`index.ts:236-264`).
fn resolve_color(
    theme: &ThemeJson,
    mode: Mode,
    value: &ColorValue,
    chain: &mut Vec<String>,
) -> Result<Rgba, ResolveError> {
    let scalar = match value {
        ColorValue::Scalar(scalar) => scalar,
        ColorValue::Variant { dark, light } => match mode {
            Mode::Dark => dark,
            Mode::Light => light,
        },
    };
    match scalar {
        ScalarColor::Literal(color) => Ok(*color),
        ScalarColor::Number(code) => Ok(ansi_to_rgba(*code as i64)),
        ScalarColor::Malformed(text) => Err(ResolveError::Malformed(text.clone())),
        ScalarColor::Reference(name) => {
            if chain.iter().any(|seen| seen == name) {
                let mut cycle = chain.clone();
                cycle.push(name.clone());
                return Err(ResolveError::Circular(cycle));
            }
            // `index.ts:254`: `defs` wins over a same-named theme key.
            let next = theme
                .defs
                .get(name)
                .or_else(|| theme.theme.get(name))
                .ok_or_else(|| ResolveError::UnknownReference(name.clone()))?;
            chain.push(name.clone());
            let resolved = resolve_color(theme, mode, next, chain);
            chain.pop();
            resolved
        }
    }
}

/// The foreground to draw on a selected list row (`index.ts:98-110`).
///
/// `bg` is the colour actually behind the row, used only when the theme's own
/// background is transparent and therefore carries no contrast information.
#[must_use]
pub fn selected_foreground(palette: &Palette, bg: Option<Rgba>) -> Rgba {
    if palette.has_selected_list_item_text {
        return palette.selected_list_item_text;
    }
    if palette.background.a == 0 {
        let target = bg.unwrap_or(palette.primary);
        return if target.luminance() > 0.5 * 255.0 {
            Rgba::opaque(0, 0, 0)
        } else {
            Rgba::opaque(255, 255, 255)
        };
    }
    palette.background
}

/// The palette used when even the built-in default theme cannot be resolved.
///
/// This exists so that resolution has a total base case and can never recurse or
/// panic looking for a fallback. It is deliberately colourless: ANSI white on the
/// terminal's own background, which is legible in every emulator.
fn last_resort() -> Palette {
    let mut palette = Palette::uniform(ANSI_16[7]);
    for slot in [
        &mut palette.background,
        &mut palette.background_panel,
        &mut palette.background_element,
        &mut palette.background_menu,
        &mut palette.diff_added_bg,
        &mut palette.diff_removed_bg,
        &mut palette.diff_context_bg,
        &mut palette.diff_added_line_number_bg,
        &mut palette.diff_removed_line_number_bg,
    ] {
        *slot = Rgba::TRANSPARENT;
    }
    palette
}

/// The built-in default theme's palette, the source every per-key fallback draws
/// from.
fn baseline(mode: Mode) -> &'static Palette {
    static DARK: OnceLock<Palette> = OnceLock::new();
    static LIGHT: OnceLock<Palette> = OnceLock::new();
    let cell = match mode {
        Mode::Dark => &DARK,
        Mode::Light => &LIGHT,
    };
    cell.get_or_init(|| {
        BUILTIN_THEME_SOURCES
            .iter()
            .find(|(name, _)| *name == DEFAULT_THEME)
            .and_then(|(_, source)| ThemeJson::parse(source).ok())
            .map_or_else(last_resort, |theme| {
                resolve_palette(&theme, mode, &last_resort()).0
            })
    })
}

// ---------------------------------------------------------------------------
// Built-in themes
// ---------------------------------------------------------------------------

/// How many themes ship in the binary.
///
/// Asserted against the embedded table so a dropped asset fails a test instead of
/// quietly shrinking the theme list.
pub const BUILTIN_THEME_COUNT: usize = 34;

/// The oracle's `packages/tui/src/theme/assets/` directory, embedded.
///
/// Names are the keys `DEFAULT_THEMES` publishes (`index.ts:127-164`), which differ
/// from the file stems only in that the oracle spells them explicitly for the
/// hyphenated ones.
static BUILTIN_THEME_SOURCES: [(&str, &str); BUILTIN_THEME_COUNT] = [
    ("aura", include_str!("../assets/themes/aura.json")),
    ("ayu", include_str!("../assets/themes/ayu.json")),
    ("carbonfox", include_str!("../assets/themes/carbonfox.json")),
    (
        "catppuccin",
        include_str!("../assets/themes/catppuccin.json"),
    ),
    (
        "catppuccin-frappe",
        include_str!("../assets/themes/catppuccin-frappe.json"),
    ),
    (
        "catppuccin-macchiato",
        include_str!("../assets/themes/catppuccin-macchiato.json"),
    ),
    ("cobalt2", include_str!("../assets/themes/cobalt2.json")),
    ("cursor", include_str!("../assets/themes/cursor.json")),
    ("dracula", include_str!("../assets/themes/dracula.json")),
    (
        "everforest",
        include_str!("../assets/themes/everforest.json"),
    ),
    ("flexoki", include_str!("../assets/themes/flexoki.json")),
    ("github", include_str!("../assets/themes/github.json")),
    ("gruvbox", include_str!("../assets/themes/gruvbox.json")),
    ("kanagawa", include_str!("../assets/themes/kanagawa.json")),
    (
        "lucent-orng",
        include_str!("../assets/themes/lucent-orng.json"),
    ),
    ("material", include_str!("../assets/themes/material.json")),
    ("matrix", include_str!("../assets/themes/matrix.json")),
    ("mercury", include_str!("../assets/themes/mercury.json")),
    ("monokai", include_str!("../assets/themes/monokai.json")),
    ("nightowl", include_str!("../assets/themes/nightowl.json")),
    ("nord", include_str!("../assets/themes/nord.json")),
    ("one-dark", include_str!("../assets/themes/one-dark.json")),
    ("zuno", include_str!("../assets/themes/zuno.json")),
    ("orng", include_str!("../assets/themes/orng.json")),
    (
        "osaka-jade",
        include_str!("../assets/themes/osaka-jade.json"),
    ),
    ("palenight", include_str!("../assets/themes/palenight.json")),
    ("rosepine", include_str!("../assets/themes/rosepine.json")),
    ("solarized", include_str!("../assets/themes/solarized.json")),
    (
        "synthwave84",
        include_str!("../assets/themes/synthwave84.json"),
    ),
    ("system", include_str!("../assets/themes/system.json")),
    (
        "tokyonight",
        include_str!("../assets/themes/tokyonight.json"),
    ),
    ("vercel", include_str!("../assets/themes/vercel.json")),
    ("vesper", include_str!("../assets/themes/vesper.json")),
    ("zenburn", include_str!("../assets/themes/zenburn.json")),
];

/// The built-in theme names, in the order they are embedded.
#[must_use]
pub fn builtin_theme_names() -> Vec<&'static str> {
    BUILTIN_THEME_SOURCES
        .iter()
        .map(|(name, _)| *name)
        .collect()
}

// ---------------------------------------------------------------------------
// Terminal-derived themes
// ---------------------------------------------------------------------------

/// The colours a terminal emulator reports about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalColors {
    /// The emulator's default background, when it answered.
    pub default_background: Option<Rgba>,
    /// The emulator's default foreground, when it answered.
    pub default_foreground: Option<Rgba>,
    /// Palette entries by ANSI index; a `None` means the emulator did not answer
    /// for that slot.
    pub palette: Vec<Option<Rgba>>,
}

impl TerminalColors {
    /// Palette entry `index`, or the standard ANSI colour for it
    /// (`index.ts:366-370`).
    #[must_use]
    pub fn color(&self, index: usize) -> Rgba {
        self.palette
            .get(index)
            .copied()
            .flatten()
            .unwrap_or_else(|| ansi_to_rgba(i64::try_from(index).unwrap_or(0)))
    }
}

/// A source of terminal colour capabilities.
///
/// The escape-sequence round trip that really answers this question needs the
/// stdin/stdout pair `app::TerminalSession` owns, so this crate takes the answer as
/// an input. That also makes the whole system layer testable with a fake, the same
/// way `app::TerminalLifecycle` makes terminal restoration testable without a TTY.
pub trait TerminalPalette: Send + Sync {
    /// The terminal's colours, or `None` when the capability is unavailable.
    fn query(&self) -> Option<TerminalColors>;
}

/// A probe that reads the `COLORFGBG` convention and nothing else.
///
/// Deliberately conservative: when the variable is absent — which includes every
/// non-interactive run, and therefore every test — it reports `None` and the system
/// layer stays empty.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvironmentPalette;

impl TerminalPalette for EnvironmentPalette {
    fn query(&self) -> Option<TerminalColors> {
        let raw = std::env::var("COLORFGBG").ok()?;
        let (foreground, background) = parse_colorfgbg(&raw)?;
        Some(TerminalColors {
            default_background: Some(ansi_to_rgba(i64::from(background))),
            default_foreground: Some(ansi_to_rgba(i64::from(foreground))),
            palette: ANSI_16.iter().copied().map(Some).collect(),
        })
    }
}

/// The production probe for the terminal's foreground and background colours.
///
/// `terminal-colorsaurus` performs one bounded OSC 10/11 transaction, including
/// raw-mode restoration and terminal-support detection. The CLI calls this before
/// [`crate::app::TerminalSession`] starts, so its response bytes cannot race the
/// normal crossterm event reader. `COLORFGBG` remains the fallback for terminals
/// which deliberately decline OSC colour queries.
#[derive(Debug, Clone, Copy)]
pub struct HostTerminalPalette {
    timeout: Duration,
}

impl Default for HostTerminalPalette {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(350),
        }
    }
}

impl HostTerminalPalette {
    /// Create a probe with a bounded OSC response wait.
    #[must_use]
    pub const fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl TerminalPalette for HostTerminalPalette {
    fn query(&self) -> Option<TerminalColors> {
        let mut options = terminal_colorsaurus::QueryOptions::default();
        options.timeout = self.timeout;
        if let Ok(colors) = terminal_colorsaurus::color_palette(options) {
            let rgba = |color: &terminal_colorsaurus::Color| {
                let (r, g, b) = color.scale_to_8bit();
                Rgba::opaque(r, g, b)
            };
            return Some(TerminalColors {
                default_background: Some(rgba(&colors.background)),
                default_foreground: Some(rgba(&colors.foreground)),
                palette: ANSI_16.iter().copied().map(Some).collect(),
            });
        }
        EnvironmentPalette.query()
    }
}

/// Parse `COLORFGBG`, which is `fg;bg` or `fg;<ignored>;bg` of ANSI indices.
///
/// Kept separate from the environment read so it can be tested without mutating
/// process state, which concurrent tests cannot do safely.
#[must_use]
pub fn parse_colorfgbg(value: &str) -> Option<(u8, u8)> {
    let fields: Vec<&str> = value.split(';').collect();
    let (first, last) = match fields.as_slice() {
        [first, last] | [first, _, last] => (*first, *last),
        _ => return None,
    };
    Some((first.trim().parse().ok()?, last.trim().parse().ok()?))
}

/// Whether the terminal's background reads as light or dark (`index.ts:353-358`).
///
/// `None` when the emulator did not report a background at all, which is the
/// oracle's signal to keep whatever mode is already in effect.
#[must_use]
pub fn terminal_mode(colors: &TerminalColors) -> Option<Mode> {
    let background = colors.default_background?;
    Some(if background.luminance() > 0.5 * 255.0 {
        Mode::Light
    } else {
        Mode::Dark
    })
}

/// Derive a theme from the terminal's own colours (`index.ts:360-469`).
///
/// Returns `None` when the terminal reported no palette entry zero, which is the
/// condition `src/context/theme.tsx:155-160` treats as "no system theme".
#[must_use]
pub fn derive_system_theme(colors: &TerminalColors, mode: Mode) -> Option<ThemeJson> {
    if colors.palette.first().copied().flatten().is_none() && colors.default_background.is_none() {
        return None;
    }
    let bg = colors.default_background.unwrap_or_else(|| colors.color(0));
    let fg = colors.default_foreground.unwrap_or_else(|| colors.color(7));
    let transparent = bg.with_alpha(0);
    let is_dark = mode == Mode::Dark;

    let grays = gray_scale(bg, is_dark);
    let gray = |step: usize| grays[step - 1];
    let text_muted = muted_text(bg, is_dark);

    let black = colors.color(0);
    let red = colors.color(1);
    let green = colors.color(2);
    let yellow = colors.color(3);
    let blue = colors.color(4);
    let magenta = colors.color(5);
    let cyan = colors.color(6);
    let red_bright = colors.color(9);
    let green_bright = colors.color(10);
    // `index.ts:378` and `:385` bind black and white for symmetry with the other
    // ANSI slots even though only some are consumed; keeping the binding documents
    // that the omission is the oracle's, not ours.
    let _ = black;

    let diff_alpha = if is_dark { 0.22 } else { 0.14 };
    let diff_added_bg = tint(bg, green, diff_alpha);
    let diff_removed_bg = tint(bg, red, diff_alpha);
    let diff_context_bg = gray(2);

    Some(ThemeJson::from_literals(vec![
        ("primary", cyan),
        ("secondary", text_muted),
        ("accent", cyan),
        ("error", red),
        ("warning", yellow),
        ("success", green),
        ("info", cyan),
        ("text", fg),
        ("textMuted", text_muted),
        ("selectedListItemText", bg),
        ("background", transparent),
        ("backgroundPanel", gray(2)),
        ("backgroundElement", gray(3)),
        ("backgroundMenu", gray(3)),
        ("borderSubtle", gray(6)),
        ("border", gray(7)),
        ("borderActive", gray(8)),
        ("diffAdded", green),
        ("diffRemoved", red),
        ("diffContext", gray(7)),
        ("diffHunkHeader", gray(7)),
        ("diffHighlightAdded", green_bright),
        ("diffHighlightRemoved", red_bright),
        ("diffAddedBg", diff_added_bg),
        ("diffRemovedBg", diff_removed_bg),
        ("diffContextBg", diff_context_bg),
        ("diffLineNumber", text_muted),
        (
            "diffAddedLineNumberBg",
            tint(diff_context_bg, green, diff_alpha),
        ),
        (
            "diffRemovedLineNumberBg",
            tint(diff_context_bg, red, diff_alpha),
        ),
        ("markdownText", fg),
        ("markdownHeading", fg),
        ("markdownLink", blue),
        ("markdownLinkText", cyan),
        ("markdownCode", fg),
        ("markdownBlockQuote", yellow),
        ("markdownEmph", yellow),
        ("markdownStrong", fg),
        ("markdownHorizontalRule", gray(7)),
        ("markdownListItem", text_muted),
        ("markdownListEnumeration", cyan),
        ("markdownImage", blue),
        ("markdownImageText", cyan),
        ("markdownCodeBlock", fg),
        ("syntaxComment", text_muted),
        ("syntaxKeyword", magenta),
        ("syntaxFunction", blue),
        ("syntaxVariable", fg),
        ("syntaxString", green),
        ("syntaxNumber", yellow),
        ("syntaxType", cyan),
        ("syntaxOperator", cyan),
        ("syntaxPunctuation", fg),
    ]))
}

/// Twelve steps away from the terminal background (`index.ts:471-523`).
///
/// Returned zero-indexed; the oracle's `grays[1]` is `[0]`.
fn gray_scale(bg: Rgba, is_dark: bool) -> [Rgba; 12] {
    let luminance = bg.luminance();
    let mut steps = [Rgba::TRANSPARENT; 12];
    for (index, slot) in steps.iter_mut().enumerate() {
        let factor = (index + 1) as f64 / 12.0;
        let channels = if is_dark {
            if luminance < 10.0 {
                let gray = (factor * 0.4 * 255.0).floor();
                [gray, gray, gray]
            } else {
                let ratio = (luminance + (255.0 - luminance) * factor * 0.4) / luminance;
                [
                    (f64::from(bg.r) * ratio).min(255.0),
                    (f64::from(bg.g) * ratio).min(255.0),
                    (f64::from(bg.b) * ratio).min(255.0),
                ]
            }
        } else if luminance > 245.0 {
            let gray = 255.0 - factor * 0.4 * 255.0;
            [gray, gray, gray]
        } else {
            let ratio = 1.0 - factor * 0.4;
            [
                (f64::from(bg.r) * ratio).max(0.0),
                (f64::from(bg.g) * ratio).max(0.0),
                (f64::from(bg.b) * ratio).max(0.0),
            ]
        };
        let floor = |value: f64| value.floor().clamp(0.0, 255.0) as u8;
        *slot = Rgba::opaque(floor(channels[0]), floor(channels[1]), floor(channels[2]));
    }
    steps
}

/// The muted text colour for a given terminal background (`index.ts:525-554`).
fn muted_text(bg: Rgba, is_dark: bool) -> Rgba {
    let luminance = bg.luminance();
    let gray = if is_dark {
        if luminance < 10.0 {
            180.0
        } else {
            (160.0 + luminance * 0.3).floor().min(200.0)
        }
    } else if luminance > 245.0 {
        75.0
    } else {
        (100.0 - (255.0 - luminance) * 0.2).floor().max(60.0)
    };
    let gray = gray.clamp(0.0, 255.0) as u8;
    Rgba::opaque(gray, gray, gray)
}

/// What a system-theme refresh concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemThemeOutcome {
    /// The terminal answered and the `system` theme now exists in the top layer.
    Derived(Mode),
    /// The terminal reported nothing usable; the system layer is empty.
    Unavailable,
}

// ---------------------------------------------------------------------------
// The four-layer registry
// ---------------------------------------------------------------------------

/// A fully resolved theme, plus everything that went wrong resolving it.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The theme actually resolved, which differs from the requested name when a
    /// fallback happened.
    pub name: String,
    /// The mode it was resolved for.
    pub mode: Mode,
    /// The colours a view paints with.
    pub palette: Palette,
    /// Recoverable problems, in the order they were found.
    pub issues: Vec<ThemeIssue>,
}

impl Resolved {
    /// The issues formatted for a log line, each naming its theme and key.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<String> {
        self.issues
            .iter()
            .map(|issue| format!("theme {:?}: {issue}", self.name))
            .collect()
    }
}

/// The four theme layers and the precedence between them.
///
/// Highest wins: system, then user custom files, then plugin-provided, then
/// built-in (`index.ts:171-183`).
#[derive(Debug, Clone)]
pub struct ThemeRegistry {
    builtin: BTreeMap<&'static str, ThemeJson>,
    plugin: BTreeMap<String, ThemeJson>,
    custom: BTreeMap<String, ThemeJson>,
    system: Option<ThemeJson>,
    load_issues: Vec<String>,
}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ThemeRegistry {
    /// A registry holding only the built-in themes.
    ///
    /// An asset that fails to parse is reported through [`Self::load_issues`]
    /// rather than panicking, so a corrupt embed degrades to "that theme is
    /// missing" instead of preventing startup.
    #[must_use]
    pub fn new() -> Self {
        let mut builtin = BTreeMap::new();
        let mut load_issues = Vec::new();
        for (name, source) in &BUILTIN_THEME_SOURCES {
            match ThemeJson::parse(source) {
                Ok(theme) => {
                    builtin.insert(*name, theme);
                }
                Err(error) => load_issues.push(format!("built-in theme {name:?}: {error}")),
            }
        }
        Self {
            builtin,
            plugin: BTreeMap::new(),
            custom: BTreeMap::new(),
            system: None,
            load_issues,
        }
    }

    /// Built-in assets that could not be parsed. Empty in a correct build.
    #[must_use]
    pub fn load_issues(&self) -> &[String] {
        &self.load_issues
    }

    /// How many built-in themes parsed.
    #[must_use]
    pub fn builtin_count(&self) -> usize {
        self.builtin.len()
    }

    /// The definition that wins for `name`, searching layers highest first.
    #[must_use]
    pub fn definition(&self, name: &str) -> Option<&ThemeJson> {
        // The system layer publishes exactly one name (`index.ts:179-182`), so it
        // can shadow `system` and nothing else.
        if name == SYSTEM_THEME
            && let Some(theme) = self.system.as_ref()
        {
            return Some(theme);
        }
        self.custom
            .get(name)
            .or_else(|| self.plugin.get(name))
            .or_else(|| self.builtin.get(name))
    }

    /// Which layer supplies `name`, for diagnostics and tests.
    #[must_use]
    pub fn layer_of(&self, name: &str) -> Option<ThemeLayer> {
        if name == SYSTEM_THEME && self.system.is_some() {
            return Some(ThemeLayer::System);
        }
        if self.custom.contains_key(name) {
            return Some(ThemeLayer::Custom);
        }
        if self.plugin.contains_key(name) {
            return Some(ThemeLayer::Plugin);
        }
        if self.builtin.contains_key(name) {
            return Some(ThemeLayer::Builtin);
        }
        None
    }

    /// Whether any layer provides `name` (`index.ts:215-218`).
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        !name.is_empty() && self.definition(name).is_some()
    }

    /// Every selectable theme name, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: BTreeSet<String> = self
            .builtin
            .keys()
            .map(|name| (*name).to_owned())
            .chain(self.plugin.keys().cloned())
            .chain(self.custom.keys().cloned())
            .collect();
        if self.system.is_some() {
            names.insert(SYSTEM_THEME.to_owned());
        }
        names.into_iter().collect()
    }

    /// Install a plugin-provided theme, refusing to shadow an existing name.
    ///
    /// This is the oracle's `addTheme` (`index.ts:220-227`): a plugin may add, but
    /// not silently replace.
    pub fn add_plugin_theme(&mut self, name: &str, theme: ThemeJson) -> bool {
        if name.is_empty() || self.has(name) {
            return false;
        }
        self.plugin.insert(name.to_owned(), theme);
        true
    }

    /// Install or replace a theme, writing to whichever of the custom and plugin
    /// layers already holds the name (`index.ts:229-240`).
    pub fn upsert_theme(&mut self, name: &str, theme: ThemeJson) -> bool {
        if name.is_empty() {
            return false;
        }
        if self.custom.contains_key(name) {
            self.custom.insert(name.to_owned(), theme);
        } else {
            self.plugin.insert(name.to_owned(), theme);
        }
        true
    }

    /// Replace the whole user-custom layer (`index.ts:205-208`).
    pub fn set_custom_themes(&mut self, themes: BTreeMap<String, ThemeJson>) {
        self.custom = themes;
    }

    /// Replace the terminal-derived layer (`index.ts:210-213`).
    pub fn set_system_theme(&mut self, theme: Option<ThemeJson>) {
        self.system = theme;
    }

    /// Re-derive the system layer from a terminal probe.
    ///
    /// `locked` is the user's pinned mode, which wins over the terminal's own
    /// reading; `fallback` is used when neither says anything
    /// (`src/context/theme.tsx:165`).
    pub fn refresh_system_theme(
        &mut self,
        probe: &dyn TerminalPalette,
        locked: Option<Mode>,
        fallback: Mode,
    ) -> SystemThemeOutcome {
        let Some(colors) = probe.query() else {
            self.set_system_theme(None);
            return SystemThemeOutcome::Unavailable;
        };
        let mode = locked
            .or_else(|| terminal_mode(&colors))
            .unwrap_or(fallback);
        match derive_system_theme(&colors, mode) {
            Some(theme) => {
                self.set_system_theme(Some(theme));
                SystemThemeOutcome::Derived(mode)
            }
            None => {
                self.set_system_theme(None);
                SystemThemeOutcome::Unavailable
            }
        }
    }

    /// Resolve `name` into a palette, falling back with a diagnostic when the name
    /// or any of its keys is absent.
    #[must_use]
    pub fn resolve(&self, name: &str, mode: Mode) -> Resolved {
        let (definition, resolved_name, mut issues) = match self.definition(name) {
            Some(definition) => (Some(definition), name.to_owned(), Vec::new()),
            None => (
                self.definition(DEFAULT_THEME),
                DEFAULT_THEME.to_owned(),
                vec![ThemeIssue::UnknownTheme {
                    requested: name.to_owned(),
                }],
            ),
        };
        let palette = match definition {
            Some(definition) => {
                let (palette, mut found) = resolve_palette(definition, mode, baseline(mode));
                issues.append(&mut found);
                palette
            }
            // Only reachable if the default theme's own asset failed to parse,
            // which `load_issues` already reported.
            None => baseline(mode).clone(),
        };
        Resolved {
            name: resolved_name,
            mode,
            palette,
            issues,
        }
    }

    /// Resolve whatever the `theme` configuration key selected.
    ///
    /// `None` means the key was absent, which selects [`DEFAULT_THEME`]; the value
    /// `system` selects the terminal-derived layer when a probe has filled it, and
    /// otherwise falls back with a diagnostic.
    #[must_use]
    pub fn resolve_configured(&self, configured: Option<&str>, mode: Mode) -> Resolved {
        self.resolve(configured.unwrap_or(DEFAULT_THEME), mode)
    }
}

/// Which of the four layers a theme came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThemeLayer {
    /// Embedded in the binary.
    Builtin,
    /// Installed by a plugin.
    Plugin,
    /// A user's own theme file.
    Custom,
    /// Derived from the terminal's reported colours.
    System,
}

// ---------------------------------------------------------------------------
// A view that paints only from the palette
// ---------------------------------------------------------------------------

/// A one-row-per-colour preview of a resolved palette.
///
/// It exists for two reasons. It is the theme picker's preview, and it is the
/// snapshot subject that proves a palette change is visible in rendered output —
/// which requires that every field reach the screen, so every field gets a row.
#[derive(Debug, Clone)]
pub struct PaletteSampleView {
    palette: Palette,
    title: String,
}

/// Columns [`PaletteSampleView`] needs: the longest key plus a leading marker.
pub const SAMPLE_VIEW_WIDTH: u16 = 26;

impl PaletteSampleView {
    /// A preview of `resolved`.
    #[must_use]
    pub fn new(resolved: &Resolved) -> Self {
        Self {
            palette: resolved.palette.clone(),
            title: resolved.name.clone(),
        }
    }

    /// Rows this view needs: a title, every colour, the opacity, and the derived
    /// selection foreground.
    #[must_use]
    pub fn height(&self) -> u16 {
        u16::try_from(self.palette.entries().len() + 3).unwrap_or(u16::MAX)
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let background = ratatui::style::Color::from(self.palette.background);
        let mut lines = vec![Line::from(Span::styled(
            format!("{:<width$}", self.title, width = SAMPLE_VIEW_WIDTH as usize),
            Style::new()
                .fg(self.palette.text.into())
                .bg(background)
                .add_modifier(Modifier::BOLD),
        ))];
        for (key, color) in self.palette.entries() {
            lines.push(Line::from(Span::styled(
                format!("{key:<width$}", width = SAMPLE_VIEW_WIDTH as usize),
                Style::new().fg(color.into()).bg(background),
            )));
        }
        lines.push(Line::from(Span::styled(
            format!(
                "{:<width$}",
                format!("thinkingOpacity {:.2}", self.palette.thinking_opacity),
                width = SAMPLE_VIEW_WIDTH as usize
            ),
            Style::new()
                .fg(self.palette.text_muted.into())
                .bg(background),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "{:<width$}",
                "selectedForeground",
                width = SAMPLE_VIEW_WIDTH as usize
            ),
            Style::new()
                .fg(selected_foreground(&self.palette, None).into())
                .bg(self.palette.primary.into()),
        )));
        lines
    }
}

impl Component for PaletteSampleView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        Paragraph::new(self.lines()).render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, _event: &crate::app::AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

#[cfg(test)]
#[path = "theme_tests.rs"]
mod tests;

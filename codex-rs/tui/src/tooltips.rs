use crate::keymap::RuntimeKeymap;
use crate::keymap::keymap_action_id;
use crate::terminal_hyperlinks::HyperlinkLine;
use codex_config::types::TuiKeymap;
use codex_features::FEATURES;
use codex_features::Feature;
use codex_features::FeatureSpec;
use codex_protocol::account::PlanType;
use lazy_static::lazy_static;
use rand::Rng;
use rand::seq::IteratorRandom;
use std::path::Path;

#[cfg(test)]
#[path = "tooltips/keybinding_tests.rs"]
mod keybinding_tests;

const RAW_TOOLTIPS: &str = include_str!("../assets/tooltips.txt");

lazy_static! {
    static ref TOOLTIPS: Vec<&'static str> = RAW_TOOLTIPS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    static ref ALL_TOOLTIPS: Vec<&'static str> = {
        let mut tips = Vec::new();
        tips.extend(TOOLTIPS.iter().copied());
        tips.extend(experimental_tooltips(
            FEATURES,
            codex_realtime_webrtc::RealtimeWebrtcSession::is_supported,
        ));
        tips
    };
}

fn experimental_tooltips(
    features: &[FeatureSpec],
    voice_supported: impl Fn() -> bool,
) -> Vec<&'static str> {
    features
        .iter()
        .filter(|spec| spec.id != Feature::RealtimeConversation || voice_supported())
        .filter_map(|spec| spec.stage.experimental_announcement())
        .collect()
}

/// Pick a random, locally bundled tooltip to show when starting Zuno.
///
/// Zuno deliberately does not fetch inherited upstream announcements or select
/// subscription and Desktop-app marketing based on the account plan, so the plan
/// and Fast mode inputs are accepted for signature parity and otherwise ignored.
pub(crate) fn get_tooltip(
    _plan: Option<PlanType>,
    _fast_mode_enabled: bool,
    keymap: &TuiKeymap,
) -> Option<String> {
    pick_tooltip(&mut rand::rng(), keymap)
}

fn pick_tooltip<R: Rng + ?Sized>(rng: &mut R, keymap: &TuiKeymap) -> Option<String> {
    // Resolve current settings for each new tip; never replace an invalid or unbound keymap
    // with defaults, or cache shortcut text across /keymap edits.
    let keymap = RuntimeKeymap::from_config(keymap).ok();
    resolved_tooltips(keymap.as_ref()).choose(rng)
}

/// Render shared tip styling and links, retaining visible URLs when the terminal needs them.
pub(crate) fn render_tooltip_lines(tip: &str, width: usize, cwd: &Path) -> Vec<HyperlinkLine> {
    crate::markdown_render::render_streaming_markdown_lines_with_width_and_cwd(
        &format!("**Tip:** {tip}"),
        Some(width),
        Some(cwd),
        &crate::markdown_render::hide_web_link_destination,
        crate::markdown_render::ListSpacing::AfterMultiline,
    )
    .lines
}

/// Resolve the local tip pool in catalog order using the supplied runtime keymap.
/// Tips with invalid or unbound shortcuts are omitted; without a keymap, only key-free tips remain.
pub(crate) fn resolved_tooltips(
    keymap: Option<&RuntimeKeymap>,
) -> impl Iterator<Item = String> + '_ {
    ALL_TOOLTIPS
        .iter()
        .filter_map(move |tip| render_tooltip(tip, keymap))
}

/// Substitute `{key:context.action}` with the current primary shortcut in a Markdown code span.
/// Skip the tip if a placeholder is invalid or its action has no binding.
fn render_tooltip(mut template: &str, keymap: Option<&RuntimeKeymap>) -> Option<String> {
    let mut rendered = String::new();
    while let Some((prefix, rest)) = template.split_once("{key:") {
        let (action, suffix) = rest.split_once('}')?;
        let (context, action) = action.split_once('.')?;
        let action = keymap_action_id(context, action)?;
        let hint = keymap?.primary_hint(action.context, action.action)?;
        rendered.push_str(prefix);
        // A key or two-key chord can contain literal backticks; use a padded code span.
        rendered.push_str(&format!("`` {} ``", hint.display_label()));
        template = suffix;
    }
    rendered.push_str(template);
    Some(rendered)
}

/// Upstream Codex prewarms and reads a remote announcement feed here. Zuno keeps the
/// call site so the composer hint policy stays structurally identical, but never
/// fetches inherited upstream announcements, so there is nothing to prefer over the
/// locally bundled tips.
pub(crate) mod announcement {
    use codex_protocol::account::PlanType;

    /// Always `None`: Zuno has no announcement feed to consult.
    pub(crate) fn fetch_announcement_tip(_plan: Option<PlanType>) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn experimental_voice_tooltip_requires_runtime_support() {
        let mut features = FEATURES.to_vec();
        features
            .iter_mut()
            .find(|spec| spec.id == Feature::RealtimeConversation)
            .unwrap()
            .stage = codex_features::Stage::Experimental {
            name: "Voice conversations",
            menu_description: "Talk with Zuno using /voice.",
            announcement: "NEW: Voice conversations can now be enabled from /experimental. Restart Zuno after enabling, then use /voice.",
        };
        let unavailable = experimental_tooltips(&features, || false);
        let available = experimental_tooltips(&features, || true);
        let voice_tip = features
            .iter()
            .find(|spec| spec.id == Feature::RealtimeConversation)
            .and_then(|spec| spec.stage.experimental_announcement())
            .expect("voice has an experimental announcement");
        assert_eq!(
            unavailable,
            available
                .iter()
                .copied()
                .filter(|tip| *tip != voice_tip)
                .collect::<Vec<_>>()
        );
        insta::assert_snapshot!(
            "experimental_voice_tooltip",
            available
                .into_iter()
                .filter(|tip| *tip == voice_tip)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn random_tooltip_returns_some_tip_when_available() {
        let mut rng = StdRng::seed_from_u64(42);
        assert!(pick_tooltip(&mut rng, &TuiKeymap::default()).is_some());
    }

    #[test]
    fn random_tooltip_is_reproducible_with_seed() {
        let expected = {
            let mut rng = StdRng::seed_from_u64(7);
            pick_tooltip(&mut rng, &TuiKeymap::default())
        };

        let mut rng = StdRng::seed_from_u64(7);
        assert_eq!(expected, pick_tooltip(&mut rng, &TuiKeymap::default()));
    }

    #[test]
    fn bundled_tooltips_do_not_market_inherited_codex_products() {
        for tooltip in TOOLTIPS.iter() {
            let lower = tooltip.to_ascii_lowercase();
            for forbidden in [
                "codex app",
                "codex desktop",
                "chatgpt.com/codex",
                "learn.chatgpt.com",
                "discord.gg/openai",
                "community.openai.com/c/codex",
            ] {
                assert!(
                    !lower.contains(forbidden),
                    "tooltip contains inherited product marketing `{forbidden}`: {tooltip}"
                );
            }
        }
    }
}

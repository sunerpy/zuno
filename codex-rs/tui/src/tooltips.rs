use crate::keymap::RuntimeKeymap;
use crate::keymap::keymap_action_id;
use crate::terminal_hyperlinks::HyperlinkLine;
use codex_config::types::TuiKeymap;
use codex_features::FEATURES;
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
        tips.extend(
            FEATURES
                .iter()
                .filter_map(|spec| spec.stage.experimental_announcement()),
        );
        tips
    };
}

/// Pick a random, locally bundled tooltip to show when starting Zuno.
///
/// Zuno deliberately does not fetch inherited upstream announcements or select
/// subscription and Desktop-app marketing based on the account plan, so the plan
/// input is accepted for signature parity and otherwise ignored.
pub(crate) fn get_tooltip(_plan: Option<PlanType>, keymap: &TuiKeymap) -> Option<String> {
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

pub(crate) fn tooltip_templates() -> impl Iterator<Item = &'static str> {
    ALL_TOOLTIPS.iter().copied()
}

/// Substitute `{key:context.action}` with the current primary shortcut in a Markdown code span.
/// Skip the tip if a placeholder is invalid or its action has no binding.
pub(crate) fn render_tooltip(mut template: &str, keymap: Option<&RuntimeKeymap>) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

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

use codex_protocol::account::PlanType;
use lazy_static::lazy_static;
use rand::Rng;

const RAW_TOOLTIPS: &str = include_str!("../assets/tooltips.txt");

lazy_static! {
    static ref TOOLTIPS: Vec<&'static str> = RAW_TOOLTIPS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
}

/// Pick a random, locally bundled tooltip to show when starting Zuno.
///
/// Zuno deliberately does not fetch inherited upstream announcements or select
/// subscription and Desktop-app marketing based on the account plan.
pub(crate) fn get_tooltip(_plan: Option<PlanType>, _fast_mode_enabled: bool) -> Option<String> {
    pick_tooltip(&mut rand::rng()).map(str::to_string)
}

fn pick_tooltip<R: Rng + ?Sized>(rng: &mut R) -> Option<&'static str> {
    if TOOLTIPS.is_empty() {
        None
    } else {
        TOOLTIPS.get(rng.random_range(0..TOOLTIPS.len())).copied()
    }
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
        assert!(pick_tooltip(&mut rng).is_some());
    }

    #[test]
    fn random_tooltip_is_reproducible_with_seed() {
        let expected = {
            let mut rng = StdRng::seed_from_u64(7);
            pick_tooltip(&mut rng)
        };

        let mut rng = StdRng::seed_from_u64(7);
        assert_eq!(expected, pick_tooltip(&mut rng));
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

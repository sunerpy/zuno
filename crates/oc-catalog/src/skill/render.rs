//! The two model-facing render forms.
//!
//! Port of `Skill.fmt` (`packages/opencode/src/skill/index.ts:321-346`). These
//! bytes go straight into the system prompt (`session/system.ts:99-110` calls the
//! verbose form on every request), so a stray newline or a changed separator
//! changes every request this agent ever makes. Both forms are snapshot-tested.
//!
//! The oracle:
//!
//! ```text
//! export function fmt(list: Info[], opts: { verbose: boolean }) {
//!   const described = list.filter((skill) => skill.description !== undefined)
//!   if (described.length === 0) return "No skills are currently available."
//!   if (opts.verbose) {
//!     return [
//!       "<available_skills>",
//!       ...described.toSorted((a, b) => a.name.localeCompare(b.name)).flatMap((skill) => [
//!         "  <skill>",
//!         `    <name>${skill.name}</name>`,
//!         `    <description>${skill.description}</description>`,
//!         `    <location>${escapeHtml(skill.location)}</location>`,
//!         "  </skill>",
//!       ]),
//!       "</available_skills>",
//!     ].join("\n")
//!   }
//!   return [
//!     "## Available Skills",
//!     ...described.toSorted((a, b) => a.name.localeCompare(b.name))
//!       .map((skill) => `- **${skill.name}**: ${skill.description}`),
//!   ].join("\n")
//! }
//! ```
//!
//! Three details are easy to lose and all three are load-bearing:
//!
//! 1. A skill with **no** `description` is dropped from both forms, but is still
//!    in `all()`. Filtering happens before the emptiness check, so a set of
//!    description-less skills renders as `No skills are currently available.`
//! 2. `join("\n")` means **no trailing newline** on either form.
//! 3. `escapeHtml` is applied to `location` **only** — never to `name` or
//!    `description`. Reproduced exactly, entity-for-entity, even though it means
//!    an unescaped `<` in a description reaches the model.

use crate::skill::Skill;

/// Which form to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `## Available Skills` followed by one `- **name**: description` per skill.
    List,
    /// The `<available_skills>` XML block used in the system prompt.
    Verbose,
}

/// What the oracle returns when nothing describable is left.
pub const NO_SKILLS: &str = "No skills are currently available.";

/// Render a skill list into one of the two model-facing forms.
///
/// Skills without a description are dropped, the rest are sorted by
/// [`locale_compare`], and the result has no trailing newline.
#[must_use]
pub fn fmt(list: &[Skill], form: Form) -> String {
    let mut described: Vec<&Skill> = list
        .iter()
        .filter(|skill| skill.description.is_some())
        .collect();
    if described.is_empty() {
        return NO_SKILLS.to_string();
    }
    described.sort_by(|left, right| locale_compare(&left.name, &right.name));

    let mut lines: Vec<String> = Vec::new();
    match form {
        Form::Verbose => {
            lines.push("<available_skills>".to_string());
            for skill in described {
                let description = skill.description.as_deref().unwrap_or_default();
                lines.push("  <skill>".to_string());
                lines.push(format!("    <name>{}</name>", skill.name));
                lines.push(format!("    <description>{description}</description>"));
                lines.push(format!(
                    "    <location>{}</location>",
                    escape_html(&skill.location)
                ));
                lines.push("  </skill>".to_string());
            }
            lines.push("</available_skills>".to_string());
        }
        Form::List => {
            lines.push("## Available Skills".to_string());
            for skill in described {
                let description = skill.description.as_deref().unwrap_or_default();
                lines.push(format!("- **{}**: {description}", skill.name));
            }
        }
    }
    lines.join("\n")
}

/// `escapeHtml` (`packages/opencode/src/util/html.ts`), entity-for-entity.
///
/// Order matters: `&` is replaced first, so the ampersands the later rules
/// introduce are not escaped twice.
#[must_use]
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The primary weight order for printable ASCII, measured from
/// `String.prototype.localeCompare` under the oracle's Node runtime.
///
/// Letters are folded to lowercase here: case is a *tertiary* difference in ICU
/// root collation, so it may only decide a comparison once every primary weight
/// has matched. `"Zebra".localeCompare("zzz") < 0` proves it — a purely
/// positional table would put `Z` after `z` and get that backwards.
const PRIMARY_ORDER: &str =
    " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";

/// `a.name.localeCompare(b.name)` for the alphabet skill names actually use.
///
/// Two levels, which is the smallest model that reproduces every case measured
/// against the oracle:
///
/// | comparison                  | oracle | why                                  |
/// |-----------------------------|--------|--------------------------------------|
/// | `"aB"` vs `"Ab"`            | `-1`   | primaries tie, first case difference |
/// | `"ab"` vs `"aB"`            | `-1`   | lowercase sorts before uppercase     |
/// | `"a-b"` vs `"a_b"`          | `1`    | `_` outranks `-`                     |
/// | `"ab-c"` vs `"abc"`         | `-1`   | punctuation is not ignorable         |
/// | `"zz"` vs `"z-z"`           | `1`    | same, from the other side            |
/// | `"a1"` vs `"aA"`            | `-1`   | digits precede letters               |
/// | `"Zebra"` vs `"zzz"`        | `-1`   | case cannot outrank a primary        |
/// | `"a"` vs `"a-"`             | `-1`   | prefix wins                          |
///
/// Characters outside [`PRIMARY_ORDER`] — anything non-ASCII, and the control
/// range — sort after every table entry, by code point. ICU gives `é` the
/// primary weight of `e` with a secondary difference; reproducing that needs a
/// collation table this port does not carry. Recorded as a known divergence; no
/// skill name in the surveyed 136 is affected, and none contains a character
/// outside `[a-z0-9_-]`.
#[must_use]
pub fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    primary_key(left)
        .cmp(&primary_key(right))
        .then_with(|| case_key(left).cmp(&case_key(right)))
}

fn primary_key(value: &str) -> Vec<u32> {
    value
        .chars()
        .map(|ch| {
            let folded = ch.to_ascii_lowercase();
            PRIMARY_ORDER
                .chars()
                .position(|candidate| candidate == folded)
                .map_or_else(
                    || {
                        u32::try_from(PRIMARY_ORDER.chars().count()).unwrap_or(u32::MAX)
                            + u32::from(ch)
                    },
                    |index| u32::try_from(index).unwrap_or(u32::MAX),
                )
        })
        .collect()
}

fn case_key(value: &str) -> Vec<u8> {
    value
        .chars()
        .map(|ch| u8::from(ch.is_ascii_uppercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn skill(name: &str, description: Option<&str>, location: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.map(str::to_string),
            location: location.to_string(),
            content: String::new(),
        }
    }

    #[test]
    fn empty_list_renders_the_sentinel() {
        assert_eq!(fmt(&[], Form::List), NO_SKILLS);
        assert_eq!(fmt(&[], Form::Verbose), NO_SKILLS);
    }

    #[test]
    fn skills_without_a_description_are_dropped_before_the_emptiness_check() {
        let only_undescribed = vec![skill("a", None, "/a/SKILL.md")];
        assert_eq!(fmt(&only_undescribed, Form::List), NO_SKILLS);
        assert_eq!(fmt(&only_undescribed, Form::Verbose), NO_SKILLS);
    }

    #[test]
    fn neither_form_ends_with_a_newline() {
        let list = vec![skill("a", Some("d"), "/a/SKILL.md")];
        assert!(!fmt(&list, Form::List).ends_with('\n'));
        assert!(!fmt(&list, Form::Verbose).ends_with('\n'));
    }

    #[test]
    fn only_location_is_html_escaped() {
        let list = vec![skill("a<b", Some("d & <e>"), "<built-in>")];
        let verbose = fmt(&list, Form::Verbose);
        assert!(verbose.contains("<name>a<b</name>"), "{verbose}");
        assert!(
            verbose.contains("<description>d & <e></description>"),
            "{verbose}"
        );
        assert!(
            verbose.contains("<location>&lt;built-in&gt;</location>"),
            "{verbose}"
        );
    }

    #[test]
    fn escape_html_does_not_double_escape() {
        assert_eq!(escape_html("&<>\"'"), "&amp;&lt;&gt;&quot;&#39;");
    }

    #[test]
    fn locale_compare_matches_every_measured_oracle_case() {
        for (left, right, expected) in [
            ("aB", "Ab", Ordering::Less),
            ("ab", "aB", Ordering::Less),
            ("Ab", "ab", Ordering::Greater),
            ("a-b", "a_b", Ordering::Greater),
            ("ab-c", "abc", Ordering::Less),
            ("zz", "z-z", Ordering::Greater),
            ("a1", "aA", Ordering::Less),
            ("a", "a-", Ordering::Less),
            ("Zebra", "zzz", Ordering::Less),
            ("apple", "Apple", Ordering::Less),
            ("_under", "-dash", Ordering::Less),
            ("1one", "a", Ordering::Less),
        ] {
            assert_eq!(
                locale_compare(left, right),
                expected,
                "{left:?} vs {right:?}"
            );
        }
    }

    #[test]
    fn locale_compare_is_a_total_order_on_the_real_alphabet() {
        let mut names = vec![
            "amazon_quick_guide",
            "add-office365",
            "design-taste-frontend-v1",
            "codegraph",
            "codegraph-release",
            "lark-im",
            "customize-opencode",
        ];
        names.sort_by(|left, right| locale_compare(left, right));
        assert_eq!(
            names,
            vec![
                "add-office365",
                "amazon_quick_guide",
                "codegraph",
                "codegraph-release",
                "customize-opencode",
                "design-taste-frontend-v1",
                "lark-im",
            ]
        );
    }
}

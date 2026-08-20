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
    let described = described_sorted(list);
    if described.is_empty() {
        return NO_SKILLS.to_string();
    }
    assemble(&described, form)
}

/// A render that had to leave skills out, and how many.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budgeted {
    /// The rendered form, never longer than the requested budget.
    pub text: String,
    /// How many skills the text describes.
    pub rendered: usize,
    /// How many describable skills did not fit.
    pub omitted: usize,
}

/// Render at most `budget` bytes of `list`, reporting what did not fit.
///
/// [`fmt`] is unbounded because the oracle's is. At 189 skills averaging ~600 bytes
/// of description that is ~113 KB prepended to **every** request in the session, so
/// the caller that puts this in a system prompt uses this instead.
///
/// Selection is cheapest-first — which fits the most names in — while the output
/// stays sorted by name, so the bytes are stable across runs. Dropping the
/// alphabetic tail instead would hide skills for no reason but their initial.
/// [`Budgeted::omitted`] is returned rather than swallowed because a skill the model
/// is never told about is indistinguishable from one that does not exist.
#[must_use]
pub fn fmt_within(list: &[Skill], form: Form, budget: usize) -> Budgeted {
    let described = described_sorted(list);
    if described.is_empty() {
        return Budgeted {
            text: NO_SKILLS.to_string(),
            rendered: 0,
            omitted: 0,
        };
    }

    let mut by_cost: Vec<(usize, &Skill)> = described
        .iter()
        .map(|skill| (entry_cost(skill, form), *skill))
        .collect();
    by_cost.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| locale_compare(&left.1.name, &right.1.name))
    });

    let mut spent = frame_cost(form);
    let mut kept: Vec<&Skill> = Vec::new();
    for (cost, skill) in by_cost {
        if spent.saturating_add(cost) > budget {
            break;
        }
        spent += cost;
        kept.push(skill);
    }
    if kept.is_empty() {
        return Budgeted {
            text: String::new(),
            rendered: 0,
            omitted: described.len(),
        };
    }
    kept.sort_by(|left, right| locale_compare(&left.name, &right.name));

    Budgeted {
        text: assemble(&kept, form),
        rendered: kept.len(),
        omitted: described.len() - kept.len(),
    }
}

fn described_sorted(list: &[Skill]) -> Vec<&Skill> {
    let mut described: Vec<&Skill> = list
        .iter()
        .filter(|skill| skill.description.is_some())
        .collect();
    described.sort_by(|left, right| locale_compare(&left.name, &right.name));
    described
}

fn assemble(described: &[&Skill], form: Form) -> String {
    let mut lines: Vec<String> = vec![open_line(form).to_string()];
    for skill in described {
        lines.extend(entry_lines(skill, form));
    }
    if let Some(close) = close_line(form) {
        lines.push(close.to_string());
    }
    lines.join("\n")
}

const fn open_line(form: Form) -> &'static str {
    match form {
        Form::Verbose => "<available_skills>",
        Form::List => "## Available Skills",
    }
}

const fn close_line(form: Form) -> Option<&'static str> {
    match form {
        Form::Verbose => Some("</available_skills>"),
        Form::List => None,
    }
}

fn entry_lines(skill: &Skill, form: Form) -> Vec<String> {
    let description = skill.description.as_deref().unwrap_or_default();
    match form {
        Form::Verbose => vec![
            "  <skill>".to_string(),
            format!("    <name>{}</name>", skill.name),
            format!("    <description>{description}</description>"),
            format!("    <location>{}</location>", escape_html(&skill.location)),
            "  </skill>".to_string(),
        ],
        Form::List => vec![format!("- **{}**: {description}", skill.name)],
    }
}

/// Bytes one entry adds to [`assemble`]'s output, including its leading newline.
fn entry_cost(skill: &Skill, form: Form) -> usize {
    entry_lines(skill, form)
        .iter()
        .map(|line| line.len() + 1)
        .sum()
}

/// Bytes [`assemble`] spends on the opening and closing lines alone.
fn frame_cost(form: Form) -> usize {
    open_line(form).len() + close_line(form).map_or(0, |close| close.len() + 1)
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

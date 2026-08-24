//! Byte-level snapshots of the model-facing render forms.
//!
//! `Skill.fmt` (`packages/opencode/src/skill/index.ts:321-346`) is unreachable
//! from any CLI command — the verbose form is only ever called from
//! `session/system.ts:108` while assembling the system prompt — so there is no
//! `opencode` invocation to diff against. What protects these bytes instead is a
//! snapshot plus a set of assertions on the individual rules, each derived from a
//! specific line of the oracle. The snapshot is the tripwire: any change to
//! spacing, indentation, tag names, or ordering shows up as a diff before it
//! reaches a prompt.

use zuno_catalog::skill::{Form, NO_SKILLS, Skill, escape_html, render, render_within};

fn skill(name: &str, description: Option<&str>, location: &str) -> Skill {
    Skill::embedded(
        name,
        description.map(str::to_string),
        location,
        String::new(),
    )
}

/// Deliberately supplied out of order, with one description-less entry and the
/// built-in's non-path location, so the snapshot pins sorting, filtering and
/// escaping at once.
fn corpus() -> Vec<Skill> {
    vec![
        skill(
            "lark-im",
            Some("飞书即时通讯：收发消息和管理群聊。"),
            "/config/.agents/skills/lark-im/SKILL.md",
        ),
        skill("quiet", None, "/config/.agents/skills/quiet/SKILL.md"),
        skill(
            "customize-zuno",
            Some("Use ONLY when the user is editing or creating Zuno's own configuration."),
            "<built-in>",
        ),
        skill(
            "amazon_quick_guide",
            Some("MUST be loaded for ANY question about Amazon Quick."),
            "/config/.config/opencode/skill/amazon_quick_guide/SKILL.md",
        ),
        skill(
            "add-office365",
            Some("Adds Office 365 Outlook connector to a Power Apps code app."),
            "/config/.config/opencode/skill/powerapps/code-apps/skills/add-office365/SKILL.md",
        ),
    ]
}

#[test]
fn list_form_bytes() {
    insta::assert_snapshot!("list_form", render(&corpus(), Form::List));
}

#[test]
fn verbose_form_bytes() {
    insta::assert_snapshot!("verbose_form", render(&corpus(), Form::Verbose));
}

#[test]
fn index_form_contains_name_description_and_exact_source() {
    let rendered = render(&corpus(), Form::Index);
    assert!(rendered.starts_with("<skill_index>"));
    assert!(rendered.contains("name=\"add-office365\""));
    assert!(rendered.contains("Adds Office 365 Outlook connector"));
    assert!(rendered.contains(
        "source=\"/config/.config/opencode/skill/powerapps/code-apps/skills/add-office365/SKILL.md\""
    ));
    assert!(rendered.contains("source=\"&lt;built-in&gt;\""));
}

#[test]
fn empty_form_bytes() {
    insta::assert_snapshot!("empty_list_form", render(&[], Form::List));
    insta::assert_snapshot!("empty_verbose_form", render(&[], Form::Verbose));
    assert_eq!(render(&[], Form::Index), NO_SKILLS);
}

/// `join("\n")` at `:337` and `:345` — neither form is newline-terminated. A
/// trailing newline here would shift every byte after the skills block in the
/// system prompt.
#[test]
fn no_form_is_newline_terminated() {
    for form in [Form::List, Form::Verbose, Form::Index] {
        let rendered = render(&corpus(), form);
        assert!(!rendered.ends_with('\n'), "{form:?}");
        assert!(!rendered.starts_with('\n'), "{form:?}");
    }
}

/// `:322` filters, `:323` then checks emptiness — in that order, so a set of
/// description-less skills is indistinguishable from an empty one.
#[test]
fn filtering_happens_before_the_emptiness_check() {
    let hidden = vec![skill("a", None, "/a"), skill("b", None, "/b")];
    assert_eq!(render(&hidden, Form::List), NO_SKILLS);
    assert_eq!(render(&hidden, Form::Verbose), NO_SKILLS);
    assert_eq!(render(&hidden, Form::Index), NO_SKILLS);
}

/// `escapeHtml` is applied to `location` only (`:333`). `name` and `description`
/// go in raw, which is surprising enough that it needs its own assertion rather
/// than only a snapshot.
#[test]
fn escaping_covers_location_and_nothing_else() {
    let list = vec![skill(
        "a<b>&c",
        Some("uses <tags> & \"quotes\""),
        "/p/<dir>/SKILL.md",
    )];
    let verbose = render(&list, Form::Verbose);
    assert!(verbose.contains("<name>a<b>&c</name>"), "{verbose}");
    assert!(
        verbose.contains("<description>uses <tags> & \"quotes\"</description>"),
        "{verbose}"
    );
    assert!(
        verbose.contains("<location>/p/&lt;dir&gt;/SKILL.md</location>"),
        "{verbose}"
    );

    let plain = render(&list, Form::List);
    assert!(
        plain.contains("- **a<b>&c**: uses <tags> & \"quotes\""),
        "{plain}"
    );
}

/// The five entity replacements of `packages/opencode/src/util/html.ts`, in the
/// order the oracle applies them.
#[test]
fn escape_html_matches_the_oracle_entity_for_entity() {
    assert_eq!(
        escape_html("& < > \" ' plain"),
        "&amp; &lt; &gt; &quot; &#39; plain"
    );
    assert_eq!(escape_html("&amp;"), "&amp;amp;");
}

/// The verbose block is exactly five lines per skill plus the two wrapper lines,
/// with two- and four-space indentation (`:330-334`).
#[test]
fn verbose_form_has_a_fixed_line_budget() {
    let list = corpus();
    let described = list.iter().filter(|s| s.description.is_some()).count();
    let rendered = render(&list, Form::Verbose);
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), described * 5 + 2);
    assert_eq!(lines.first(), Some(&"<available_skills>"));
    assert_eq!(lines.last(), Some(&"</available_skills>"));
    assert_eq!(lines[1], "  <skill>");
    assert!(lines[2].starts_with("    <name>"));
}

/// A budget large enough for everything must produce the very bytes [`render`] does.
///
/// The two functions share [`Form`]'s framing and entry layout through one private
/// helper precisely so this can hold. If it ever fails, the budgeted path has grown
/// a second opinion about the wire format and the snapshots above no longer describe
/// what reaches a prompt.
#[test]
fn an_unspent_budget_is_byte_identical_to_the_unbounded_form() {
    for form in [Form::List, Form::Verbose, Form::Index] {
        let unbounded = render(&corpus(), form);
        let budgeted = render_within(&corpus(), form, unbounded.len());
        assert_eq!(budgeted.text, unbounded, "{form:?}");
        assert_eq!(budgeted.rendered, 4, "one of the five has no description");
        assert_eq!(budgeted.omitted, 0, "{form:?}");
    }
}

#[test]
fn a_large_index_keeps_every_source_identity_by_shortening_descriptions() {
    let corpus = (0..137)
        .map(|at| {
            skill(
                &format!("skill-{at:03}"),
                Some(&"trigger ".repeat(750)),
                &format!("/skills/skill-{at:03}/SKILL.md"),
            )
        })
        .collect::<Vec<_>>();

    let budgeted = render_within(&corpus, Form::Index, 16 * 1024);

    assert_eq!(budgeted.rendered, 137);
    assert_eq!(budgeted.omitted, 0);
    assert!(budgeted.text.len() < 16 * 1024);
    assert!(budgeted.text.contains("name=\"skill-000\""));
    assert!(budgeted.text.contains("name=\"skill-136\""));
    assert!(
        budgeted
            .text
            .contains("source=\"/skills/skill-136/SKILL.md\"")
    );
    assert!(budgeted.truncated > 0);
    assert!(
        !budgeted.text.contains(&"trigger ".repeat(750)),
        "the index retained an unbounded description"
    );
}

#[test]
fn the_progressive_catalog_spends_budget_on_source_identity_before_description_detail() {
    let corpus = vec![
        skill(
            "same",
            Some(&"first source description ".repeat(200)),
            "/skills/first/SKILL.md",
        ),
        skill(
            "same",
            Some(&"second source description ".repeat(200)),
            "/skills/second/SKILL.md",
        ),
    ];

    let minimum = "<skill_index>\n  <name>same</name>\n  <name>same</name>\n</skill_index>";
    let budgeted = render_within(&corpus, Form::Index, 512);

    assert_eq!(budgeted.rendered, 2);
    assert_eq!(budgeted.omitted, 0);
    assert_ne!(budgeted.text, minimum);
    assert!(budgeted.text.contains("/skills/first/SKILL.md"));
    assert!(budgeted.text.contains("/skills/second/SKILL.md"));
    assert!(
        budgeted.text.len() <= 512,
        "descriptions must be shortened before a source identity is hidden"
    );
}

#[test]
fn a_budget_never_returns_more_bytes_than_it_was_given() {
    let full = render(&corpus(), Form::Verbose).len();
    for budget in 0..=full {
        let budgeted = render_within(&corpus(), Form::Verbose, budget);
        assert!(
            budgeted.text.len() <= budget,
            "budget {budget} produced {} bytes",
            budgeted.text.len()
        );
        assert_eq!(
            budgeted.rendered + budgeted.omitted,
            4,
            "every describable skill is either rendered or counted as omitted"
        );
    }
}

/// The trim keeps the cheapest entries, so the count that fits is the largest one.
#[test]
fn a_partial_budget_keeps_the_most_names_and_still_sorts_them_by_name() {
    let full = render(&corpus(), Form::Verbose);
    let budgeted = render_within(&corpus(), Form::Verbose, full.len() * 2 / 3);

    assert!(budgeted.omitted > 0, "two thirds must not fit everything");
    assert!(budgeted.rendered > 0, "two thirds must fit something");
    assert!(budgeted.text.starts_with("<available_skills>"));
    assert!(budgeted.text.ends_with("</available_skills>"));

    let names: Vec<&str> = budgeted
        .text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("<name>"))
        .filter_map(|line| line.strip_suffix("</name>"))
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "survivors stay in name order");
    assert_eq!(names.len(), budgeted.rendered);
}

#[test]
fn a_budget_too_small_for_one_entry_renders_nothing_rather_than_a_broken_block() {
    let budgeted = render_within(&corpus(), Form::Verbose, 1);

    assert_eq!(budgeted.text, "", "a half-open XML block would be worse");
    assert_eq!(budgeted.rendered, 0);
    assert_eq!(budgeted.omitted, 4);
}

#[test]
fn an_empty_corpus_answers_the_oracles_sentence_at_any_budget() {
    let budgeted = render_within(&[], Form::Verbose, 0);

    assert_eq!(budgeted.text, NO_SKILLS);
    assert_eq!(budgeted.rendered, 0);
    assert_eq!(budgeted.omitted, 0);
}

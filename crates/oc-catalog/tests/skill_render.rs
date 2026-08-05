//! Byte-level snapshots of the two model-facing render forms.
//!
//! `Skill.fmt` (`packages/opencode/src/skill/index.ts:321-346`) is unreachable
//! from any CLI command — the verbose form is only ever called from
//! `session/system.ts:108` while assembling the system prompt — so there is no
//! `opencode` invocation to diff against. What protects these bytes instead is a
//! snapshot plus a set of assertions on the individual rules, each derived from a
//! specific line of the oracle. The snapshot is the tripwire: any change to
//! spacing, indentation, tag names, or ordering shows up as a diff before it
//! reaches a prompt.

use oc_catalog::skill::{Form, NO_SKILLS, Skill, escape_html, render};

fn skill(name: &str, description: Option<&str>, location: &str) -> Skill {
    Skill {
        name: name.to_string(),
        description: description.map(str::to_string),
        location: location.to_string(),
        content: String::new(),
    }
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
            "customize-opencode",
            Some("Use ONLY when the user is editing or creating opencode's own configuration."),
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
fn empty_form_bytes() {
    insta::assert_snapshot!("empty_list_form", render(&[], Form::List));
    insta::assert_snapshot!("empty_verbose_form", render(&[], Form::Verbose));
}

/// `join("\n")` at `:337` and `:345` — neither form is newline-terminated. A
/// trailing newline here would shift every byte after the skills block in the
/// system prompt.
#[test]
fn no_form_is_newline_terminated() {
    for form in [Form::List, Form::Verbose] {
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

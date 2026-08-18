//! Markdown rendering tests.
//!
//! Two layers, deliberately. The [`render`] layer asserts on rows of spans, because
//! that is where the module's own arithmetic and its styles are still visible: a
//! rendered buffer reports one character per *cell*, so a wide glyph arrives as the
//! glyph plus the blank continuation cell the terminal reserved, and anything that
//! overran has already been clipped away by ratatui — an over-wide row cannot be seen
//! after rendering. The frame layer then proves those rows reach cells through the real
//! transcript, at the widths §11.6 names.
//!
//! Every frame assertion locates the row it cares about by content rather than by
//! index. A recorded lesson from this project: a frame assertion once passed vacuously
//! because a dialog covered the row under test.

use super::*;
use crate::app::render_offscreen;
use crate::theme::{Mode, Palette, ThemeRegistry};
use crate::views::ViewContext;
use crate::views::message::{Message, MessagePart, Role, TranscriptView};
use crate::views::testkit::rows;

fn palette() -> Palette {
    ThemeRegistry::new()
        .resolve(crate::theme::DEFAULT_THEME, Mode::Dark)
        .palette
}

/// The rendered rows as plain text, one string per row.
fn text(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn joined(rows: &[Row]) -> String {
    text(rows).join("\n")
}

/// Every span's content, concatenated with no separator at all.
///
/// The right shape for a content-preservation assertion. `joined` cannot serve: at width 1
/// a word occupies one row per cluster, so `stray` becomes `s\nt\nr\na\ny` and no
/// assertion about a contiguous substring can hold — while the text is plainly still
/// there. Concatenating recovers exactly the property under test, which is that no
/// character was thrown away.
fn flattened(rows: &[Row]) -> String {
    rows.iter()
        .flatten()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Whether every character of `needle` appears in `haystack`, in order.
///
/// The only content-preservation check that survives arbitrary row breaking. At eight
/// columns a fence wraps `inner` as `inn` and `er` on two rows *with the frame's own
/// `│ ` between them*, so neither a contiguous search nor a naive concatenation can see
/// the word that is plainly still on screen.
///
/// Deliberately paired with a strict contiguous assertion at a generous width, because a
/// subsequence check alone is weak: scattered letters would satisfy it. The strict half
/// proves the word is intact when there is room for it; this half proves nothing was
/// thrown away when there is not.
fn contains_in_order(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|character| character == wanted))
}

fn draw(source: &str, width: u16) -> Vec<Row> {
    render(source, width, &palette())
}

/// The first span whose content contains `needle`.
fn span_with(rows: &[Row], needle: &str) -> Span<'static> {
    rows.iter()
        .flatten()
        .find(|span| span.content.contains(needle))
        .cloned()
        .unwrap_or_else(|| panic!("no span contains {needle:?}; rendered:\n{}", joined(rows)))
}

/// An assistant transcript holding exactly `source`, ready to render.
fn transcript(source: &str) -> TranscriptView {
    let mut view = TranscriptView::new(ViewContext::defaults());
    let mut message = Message::new(Role::Assistant);
    message.parts.push(MessagePart::Text {
        text: source.to_owned(),
    });
    view.transcript_mut().push(message);
    view
}

fn frame(source: &str, width: u16, height: u16) -> Vec<String> {
    let mut view = transcript(source);
    rows(&render_offscreen(&mut view, width, height).expect("the offscreen backend is infallible"))
}

/// A reply exercising every element the plan's §7.1 table lists.
const SAMPLE: &str = "\
# Diff viewer wiring

I read the existing parser first. The **file tree** is the missing half, and
`parse()` already yields a `LineKind` per row — so the work is layout, not parsing.

## What I changed

- `views/diff.rs` — added the tree pane at a *fixed* 32 columns
- `views/session.rs` — routed the toggle through the keymap
- 中文说明：宽字符列宽已按终端列计算

1. Parse the patch
2. Build the tree

```rust
fn columns(width: u16) -> DiffColumns { Split }
```

> Side-by-side needs 100 columns.

| pane | width |
| --- | --- |
| tree | 32 |

---

See [the plan](docs/plan.md).
";

// ---------------------------------------------------------------------------
// Element coverage
// ---------------------------------------------------------------------------

#[test]
fn markdown_renders_every_element_without_its_source_punctuation() {
    let rendered = draw(SAMPLE, 80);
    let out = joined(&rendered);
    // The whole point of the task: the markup the model typed is no longer on screen.
    for source_only in ["# ", "## ", "**file tree**", "*fixed*", "```", "| --- |"] {
        assert!(
            !out.contains(source_only),
            "{source_only:?} reached the screen as literal markup:\n{out}"
        );
    }
    // …while every element it stood for did.
    for expected in [
        "Diff viewer wiring", // heading, hashes dropped
        "file tree",          // strong
        "fixed",              // emphasis
        "`parse()`",          // inline code keeps its backticks
        "• `views/diff.rs`",  // bullet list
        "1. Parse the patch", // ordered list
        "╭─ rust",            // fenced code frame with its language label
        "│ Side-by-side",     // block quote bar
        "─────",              // thematic break
        "[the plan](docs/plan.md)",
    ] {
        assert!(
            out.contains(expected),
            "{expected:?} is missing from the render:\n{out}"
        );
    }
}

#[test]
fn markdown_heading_weight_comes_from_the_palette_and_not_from_hashes() {
    // Plan §7.1: H1 bold + underline, H2 bold, H3+ colour alone. The gradient is what
    // replaces the `#` run, so if the modifiers are absent the hashes were dropped for
    // nothing and the heading is indistinguishable from prose.
    let palette = palette();
    let rendered = render("# One\n\n## Two\n\n### Three\n", 40, &palette);
    let one = span_with(&rendered, "One");
    let two = span_with(&rendered, "Two");
    let three = span_with(&rendered, "Three");
    for (name, span) in [("h1", &one), ("h2", &two), ("h3", &three)] {
        assert_eq!(
            span.style.fg,
            Some(palette.markdown_heading.into()),
            "{name} is not painted with `markdown_heading`"
        );
    }
    assert!(one.style.add_modifier.contains(Modifier::BOLD));
    assert!(one.style.add_modifier.contains(Modifier::UNDERLINED));
    assert!(two.style.add_modifier.contains(Modifier::BOLD));
    assert!(
        !two.style.add_modifier.contains(Modifier::UNDERLINED),
        "H2 is underlined too, so H1 and H2 look the same"
    );
    assert!(
        !three.style.add_modifier.contains(Modifier::BOLD),
        "H3 is bold, so it competes with H2"
    );
}

#[test]
fn markdown_emphasis_and_strong_carry_distinct_modifiers_and_colours() {
    let palette = palette();
    let rendered = render("plain *slanted* and **heavy** text\n", 60, &palette);
    let slanted = span_with(&rendered, "slanted");
    let heavy = span_with(&rendered, "heavy");
    assert!(slanted.style.add_modifier.contains(Modifier::ITALIC));
    assert!(heavy.style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(slanted.style.fg, Some(palette.markdown_emph.into()));
    assert_eq!(heavy.style.fg, Some(palette.markdown_strong.into()));
    let plain = span_with(&rendered, "plain");
    assert_eq!(plain.style.fg, Some(palette.markdown_text.into()));
    assert!(plain.style.add_modifier.is_empty());
}

#[test]
fn markdown_inline_code_keeps_its_backticks_and_its_own_colour() {
    let palette = palette();
    let rendered = render("call `render(width)` on it\n", 60, &palette);
    let code = span_with(&rendered, "render(width)");
    assert_eq!(code.content.as_ref(), "`render(width)`");
    assert_eq!(code.style.fg, Some(palette.markdown_code.into()));
}

#[test]
fn markdown_inline_code_is_never_broken_across_rows() {
    // `a b c` is one symbol to a reader; wrapping inside it puts half an identifier on
    // each of two rows, which is worse than letting the row run short.
    let rendered = draw("x `one two three four` y\n", 24);
    let out = text(&rendered);
    assert!(
        out.iter().any(|row| row.contains("`one two three four`")),
        "inline code was split across rows: {out:?}"
    );
}

#[test]
fn markdown_fence_is_framed_and_labelled_and_hugs_its_code() {
    let rendered = draw("```python\nx = 1\n```\n", 80);
    let out = text(&rendered);
    let top = out
        .iter()
        .find(|row| row.starts_with('╭'))
        .expect("no frame opened");
    assert!(
        top.contains("python"),
        "the language label is missing: {top}"
    );
    assert!(
        out.iter().any(|row| row == "│ x = 1"),
        "the code body is not inside the frame: {out:?}"
    );
    assert!(
        out.iter().any(|row| row.starts_with('╰')),
        "no frame closed"
    );
    // Sized to the code, not to the terminal: a frame that spanned 80 columns around
    // five columns of code would be furniture rather than structure.
    assert!(
        display_width(top) < 40,
        "the frame spans {} columns for a five-column line: {top}",
        display_width(top)
    );
}

#[test]
fn markdown_fence_without_a_language_still_frames_its_content() {
    let rendered = draw("```\nbare\n```\n", 40);
    let out = text(&rendered);
    assert!(out.iter().any(|row| row.starts_with('╭')));
    assert!(out.iter().any(|row| row == "│ bare"));
    assert!(out.iter().any(|row| row.starts_with('╰')));
}

#[test]
fn markdown_unknown_and_absent_fences_keep_the_plain_code_style() {
    let palette = palette();
    for source in [
        "```brainfuck\nplain unknown\n```\n",
        "```\nplain absent\n```\n",
    ] {
        let rendered = render(source, 60, &palette);
        let plain = span_with(&rendered, "plain");
        assert_eq!(
            plain.style.fg,
            Some(palette.markdown_code_block.into()),
            "an unsupported fence did not use the pre-highlighting fallback"
        );
    }
}

#[test]
fn markdown_highlighting_keeps_the_frame_label_and_cjk_comment_aligned() {
    let palette = palette();
    let source = "```rust\n// 中文注释保持对齐\nfn main() {}\n```\n";
    let rendered = render(source, 16, &palette);
    let out = text(&rendered);
    assert!(
        out.iter().any(|row| row.starts_with("╭─ rust")),
        "the P2-1 language label changed: {out:?}"
    );
    assert!(
        out.iter().any(|row| row.starts_with('╰')),
        "the P2-1 closing frame changed: {out:?}"
    );
    assert!(
        contains_in_order(&flattened(&rendered), "// 中文注释保持对齐"),
        "highlighting lost part of the CJK comment: {out:?}"
    );
    let comment = span_with(&rendered, "// ");
    assert_eq!(comment.style.fg, Some(palette.syntax_comment.into()));
    assert!(comment.style.add_modifier.contains(Modifier::ITALIC));
    for row in &rendered {
        assert!(
            row_width(row) <= 16,
            "a highlighted CJK row escaped the frame: {:?}",
            row.iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn markdown_highlight_visual_samples_preserve_text_frames_and_labels() {
    let palette = palette();
    let samples = [
        (
            "rust",
            "fn main() { println!(\"hello\"); }",
            "fn",
            palette.syntax_keyword,
        ),
        (
            "python",
            "def greet(name): return f\"hi {name}\"",
            "def",
            palette.syntax_keyword,
        ),
    ];

    for (language, code, token, colour) in samples {
        let plain = render(&format!("```plain\n{code}\n```\n"), 52, &palette);
        let highlighted = render(&format!("```{language}\n{code}\n```\n"), 52, &palette);
        let plain_text = text(&plain);
        let highlighted_text = text(&highlighted);

        eprintln!("{language} plain baseline:\n{}", plain_text.join("\n"));
        eprintln!(
            "{language} highlighted:\n{}\nstyles: {:?}",
            highlighted_text.join("\n"),
            highlighted
                .iter()
                .flatten()
                .map(|span| (
                    span.content.as_ref(),
                    span.style.fg,
                    span.style.add_modifier
                ))
                .collect::<Vec<_>>()
        );

        assert!(
            plain_text.iter().any(|row| row == &format!("│ {code}")),
            "plain baseline lost {language} source: {plain_text:?}"
        );
        assert!(
            highlighted_text
                .iter()
                .any(|row| row == &format!("│ {code}")),
            "highlighting changed {language} source: {highlighted_text:?}"
        );
        assert!(
            highlighted_text
                .iter()
                .any(|row| row.starts_with(&format!("╭─ {language}"))),
            "highlighting changed the {language} label: {highlighted_text:?}"
        );
        assert!(highlighted_text.iter().any(|row| row.starts_with('╰')));
        let styled = span_with(&highlighted, token);
        assert_eq!(styled.style.fg, Some(colour.into()));
        assert!(styled.style.add_modifier.contains(Modifier::ITALIC));
    }
}

#[test]
fn markdown_code_is_broken_not_reflowed() {
    // Indentation is meaning in most languages, so a long line is cut where it runs out
    // of columns rather than re-flowed on word boundaries.
    let rendered = draw("```\n    let alpha = beta + gamma + delta;\n```\n", 24);
    let out = text(&rendered);
    let body: Vec<&String> = out.iter().filter(|row| row.starts_with("│ ")).collect();
    assert!(body.len() >= 2, "the long line was not broken: {out:?}");
    assert!(
        body[0].starts_with("│     let"),
        "the leading indentation was reflowed away: {:?}",
        body[0]
    );
    let recovered = body
        .iter()
        .map(|row| row.trim_start_matches("│ ").to_owned())
        .collect::<Vec<_>>()
        .concat();
    assert!(
        recovered.contains("delta;"),
        "breaking the line lost its tail: {recovered:?}"
    );
}

#[test]
fn markdown_nested_lists_indent_two_columns_per_level() {
    let rendered = draw("- one\n  - two\n    - three\n", 40);
    let out = text(&rendered);
    assert_eq!(
        out.iter()
            .filter(|row| row.contains('•'))
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            String::from("• one"),
            String::from("  • two"),
            String::from("    • three"),
        ],
        "nesting does not step by {LIST_INDENT} columns: {out:?}"
    );
}

#[test]
fn markdown_a_wrapped_list_item_hangs_under_its_own_bullet() {
    // Without the hanging indent a wrapped item reads as an item followed by a
    // paragraph, which is the difference between two list entries and four.
    let rendered = draw("- alpha beta gamma delta epsilon\n- short\n", 16);
    let out = text(&rendered);
    let continuation = out
        .iter()
        .find(|row| row.contains("delta") && !row.contains('•'))
        .expect("the item did not wrap");
    assert!(
        continuation.starts_with("  "),
        "the continuation row is not aligned under the bullet: {continuation:?}"
    );
}

#[test]
fn markdown_ordered_lists_continue_the_source_numbering() {
    // A model that wrote `3.` meant to continue a list; restarting at one silently
    // renumbers its answer.
    let rendered = draw("3. third\n4. fourth\n5. fifth\n", 40);
    let out = text(&rendered);
    assert_eq!(
        out.iter()
            .filter(|row| !row.is_empty())
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            String::from("3. third"),
            String::from("4. fourth"),
            String::from("5. fifth"),
        ]
    );
}

#[test]
fn markdown_ordered_and_unordered_markers_read_from_different_tokens() {
    let palette = palette();
    let bullet = span_with(&render("- a\n", 20, &palette), "•");
    let ordinal = span_with(&render("1. a\n", 20, &palette), "1.");
    assert_eq!(bullet.style.fg, Some(palette.markdown_list_item.into()));
    assert_eq!(
        ordinal.style.fg,
        Some(palette.markdown_list_enumeration.into())
    );
}

#[test]
fn markdown_task_list_markers_replace_the_bullet() {
    let rendered = draw("- [x] done\n- [ ] pending\n", 40);
    let out = text(&rendered);
    assert!(out.iter().any(|row| row == "[x] done"), "{out:?}");
    assert!(out.iter().any(|row| row == "[ ] pending"), "{out:?}");
    assert!(
        !out.iter().any(|row| row.contains('•')),
        "the bullet and the checkbox both rendered, saying `list` twice: {out:?}"
    );
}

#[test]
fn markdown_block_quotes_prefix_every_row_and_nest() {
    let palette = palette();
    let rendered = render("> outer\n\n> > inner\n", 40, &palette);
    let out = text(&rendered);
    assert!(out.iter().any(|row| row == "│ outer"), "{out:?}");
    assert!(
        out.iter().any(|row| row == "│ │ inner"),
        "a quoted quote is not visibly deeper: {out:?}"
    );
    let bar = span_with(&rendered, "│");
    assert_eq!(
        bar.style.fg,
        Some(palette.markdown_block_quote.into()),
        "the quote bar is not painted with `markdown_block_quote`"
    );
}

#[test]
fn markdown_a_wrapped_quote_keeps_its_bar_on_every_row() {
    let rendered = draw("> alpha beta gamma delta epsilon zeta\n", 16);
    let out = text(&rendered);
    let quoted: Vec<&String> = out.iter().filter(|row| !row.is_empty()).collect();
    assert!(quoted.len() >= 2, "the quote did not wrap: {out:?}");
    for row in quoted {
        assert!(
            row.starts_with("│ "),
            "a wrapped quote row lost its bar, so the quote appears to end early: {row:?}"
        );
    }
}

#[test]
fn markdown_a_gfm_alert_keeps_the_kind_it_declared() {
    // The kind travels in the tag rather than in the text, so without re-emitting it the
    // word `WARNING` the model wrote disappears entirely.
    let rendered = draw("> [!WARNING]\n> mind the gap\n", 40);
    let out = joined(&rendered);
    assert!(
        out.contains("Warning"),
        "the alert kind was dropped:\n{out}"
    );
    assert!(out.contains("mind the gap"));
}

#[test]
fn markdown_thematic_break_fills_the_available_width() {
    let palette = palette();
    for width in [20_u16, 40, 80] {
        let rendered = render("a\n\n---\n\nb\n", width, &palette);
        let rule = rendered
            .iter()
            .find(|row| row.iter().any(|span| span.content.starts_with(RULE_GLYPH)))
            .expect("no thematic break rendered");
        assert_eq!(
            row_width(rule),
            usize::from(width),
            "the rule does not span the measure at {width} columns"
        );
        assert_eq!(
            rule[0].style.fg,
            Some(palette.markdown_horizontal_rule.into())
        );
    }
}

#[test]
fn markdown_links_keep_their_destination() {
    // A terminal cannot be clicked, so hiding the URL behind the label leaves the reader
    // with nothing to act on. This assertion exists because an earlier draft dropped it,
    // and looking at the render is what found it.
    let rendered = draw("see [the plan](docs/plan.md) first\n", 60);
    assert!(
        joined(&rendered).contains("[the plan](docs/plan.md)"),
        "the destination was lost:\n{}",
        joined(&rendered)
    );
}

#[test]
fn markdown_an_autolink_does_not_repeat_itself() {
    let rendered = draw("<https://example.com/a>\n", 60);
    let out = joined(&rendered);
    assert_eq!(
        out.matches("example.com").count(),
        1,
        "the destination was printed twice:\n{out}"
    );
}

#[test]
fn markdown_images_keep_their_alt_text_and_destination() {
    let rendered = draw("![a chart](chart.png)\n", 60);
    assert!(joined(&rendered).contains("![a chart](chart.png)"));
}

#[test]
fn markdown_strikethrough_is_a_modifier_not_tildes() {
    let rendered = draw("~~gone~~ kept\n", 40);
    let out = joined(&rendered);
    assert!(!out.contains("~~"), "the tildes reached the screen:\n{out}");
    assert!(
        span_with(&rendered, "gone")
            .style
            .add_modifier
            .contains(Modifier::CROSSED_OUT)
    );
}

// ---------------------------------------------------------------------------
// Width: terminal columns, and clusters
// ---------------------------------------------------------------------------

/// Sources whose width is only correct when measured in columns and cut on clusters.
const WIDE: [&str; 6] = [
    "中文说明：宽字符列宽必须按终端列计算而不是字符数",
    "family 👨‍👩‍👧‍👦 emoji",
    "flags 🇯🇵🇰🇷🇨🇳 in a row",
    "**日本語の強調**と`コード`",
    "- 项目一：这是一个很长的中文列表项，需要在可用宽度处换行\n- 项目二",
    "| 列 | 宽度 |\n| --- | --- |\n| 中文表头 | 一二三 |",
];

#[test]
fn markdown_no_row_ever_exceeds_the_width_it_was_given() {
    // The property `chars().count()` cannot hold, and the one §10.2 records the
    // reference implementation getting wrong. Asserted on rows rather than on a frame
    // because ratatui clips an over-wide row before a frame assertion could see it.
    // From two columns, because one column cannot hold a two-column glyph and
    // `truncate_row` documents the resulting single-cluster overflow as deliberate —
    // asserted separately below rather than papered over here.
    let palette = palette();
    let mut checked = 0;
    for source in WIDE.iter().chain(std::iter::once(&SAMPLE)) {
        for width in 2_u16..=64 {
            for row in render(source, width, &palette) {
                assert!(
                    row_width(&row) <= usize::from(width),
                    "a row measured {} columns in a {width}-column frame: {:?}",
                    row_width(&row),
                    row.iter()
                        .map(|span| span.content.as_ref())
                        .collect::<Vec<_>>()
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 2_000,
        "only {checked} rows were measured, so this scan is not exercising much"
    );
}

#[test]
fn markdown_a_single_column_frame_overflows_by_at_most_one_cluster() {
    // The documented exception, bounded. A one-column frame may emit a two-column glyph
    // because deleting it instead loses the text, but it may not emit more than that.
    let palette = palette();
    for source in WIDE.iter().chain(std::iter::once(&SAMPLE)) {
        for row in render(source, 1, &palette) {
            assert!(
                row_width(&row) <= 2,
                "a one-column frame emitted {} columns: {:?}",
                row_width(&row),
                row.iter()
                    .map(|span| span.content.as_ref())
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn markdown_never_splits_a_grapheme_cluster() {
    // `👨‍👩‍👧‍👦` is one cluster of seven scalar values. Cutting inside it renders a man, a
    // woman and two children where the author wrote a family, and `chars()`-based
    // truncation does exactly that.
    let palette = palette();
    let family = "👨‍👩‍👧‍👦";
    let mut seen = 0;
    for width in 1_u16..=24 {
        let out = joined(&render(&format!("prefix {family} suffix"), width, &palette));
        if !out.contains('\u{1f468}') {
            continue;
        }
        seen += 1;
        assert!(
            out.contains(family),
            "the family cluster was split at width {width}: {out:?}"
        );
    }
    assert!(
        seen > 0,
        "no width kept the cluster at all, so this test proved nothing"
    );
}

#[test]
fn markdown_a_wide_glyph_is_not_split_even_when_the_row_holds_one_column() {
    // One column cannot hold a two-column glyph. Emitting it anyway costs a column of
    // overflow the terminal absorbs; consuming nothing would hang the TUI, which is the
    // trade `message::wrap` already documented.
    let palette = palette();
    let rendered = render("日本語", 1, &palette);
    let out = text(&rendered);
    assert_eq!(out, vec!["日", "本", "語"], "a wide glyph was cut in half");
}

#[test]
fn markdown_table_columns_are_measured_in_terminal_columns() {
    // The §10.2 defect, in the one element that shows it: a grid sized by character
    // count puts the separator one column left of where the CJK header ends, and every
    // column after it is out of alignment.
    let rendered = draw("| 名前 | n |\n| --- | --- |\n| 日本語 | 12 |\n", 40);
    let out = text(&rendered);
    let bars: Vec<usize> = out
        .iter()
        .filter(|row| row.contains('│') || row.contains('┼'))
        .map(|row| display_width(row.split(['│', '┼']).next().unwrap_or_default()))
        .collect();
    assert!(bars.len() >= 3, "the grid did not render: {out:?}");
    assert!(
        bars.windows(2).all(|pair| pair[0] == pair[1]),
        "the separator is not in one column across rows ({bars:?}):\n{out:?}"
    );
}

// ---------------------------------------------------------------------------
// Malformed input: content survives, nothing panics
// ---------------------------------------------------------------------------

#[test]
fn markdown_an_unterminated_fence_keeps_its_code_and_still_frames_it() {
    // The normal case mid-stream, not an error: the closing marker has not arrived yet.
    let rendered = draw("intro\n\n```rust\nfn main() {}\nlet x = 1;\n", 60);
    let out = text(&rendered);
    assert!(out.iter().any(|row| row.contains("intro")));
    assert!(
        out.iter().any(|row| row.starts_with('╭')),
        "the open fence was not framed: {out:?}"
    );
    for line in ["fn main() {}", "let x = 1;"] {
        assert!(
            out.iter().any(|row| row.contains(line)),
            "{line:?} was swallowed by the open fence: {out:?}"
        );
    }
    assert!(
        out.iter().any(|row| row.starts_with('╰')),
        "the frame was left open: {out:?}"
    );
    let keyword = span_with(&rendered, "fn");
    assert_eq!(keyword.style.fg, Some(palette().syntax_keyword.into()));
}

#[test]
fn markdown_malformed_sources_never_panic_and_never_lose_a_word() {
    // Formatting is best-effort; the user's words are not. Each case pairs a source with
    // the words that must survive it whatever shape they end up in.
    let palette = palette();
    let cases: [(&str, &[&str]); 8] = [
        ("a * stray asterisk b", &["stray", "asterisk"]),
        ("unclosed **bold and then nothing", &["unclosed", "nothing"]),
        (
            "| a | b | c |\n| --- |\n| only-one |\n| x | y | z | w |",
            &["only-one", "w"],
        ),
        (
            "- one\n  - two\n    - three\n      - four\n        - five\n          - six",
            &["one", "six"],
        ),
        ("```\nouter\n```inner```\n", &["outer", "inner"]),
        ("# \n\n## \n\ntext after empty headings", &["text", "empty"]),
        ("<div onclick=\"x\">raw html</div>", &["raw", "html"]),
        ("> \n> \n>\n\nafter an empty quote", &["after", "quote"]),
    ];
    for (source, must_survive) in cases {
        // Strict first: with room to spare the word must survive intact and contiguous.
        let roomy = flattened(&render(source, 200, &palette));
        for word in must_survive {
            assert!(
                roomy.contains(word),
                "{word:?} was lost from {source:?} at 200 columns:\n{roomy}"
            );
        }
        // Then the narrow widths, where breaking the word is correct but losing it is not.
        for width in [1_u16, 8, 20, 40] {
            let out = flattened(&render(source, width, &palette));
            for word in must_survive {
                assert!(
                    contains_in_order(&out, word),
                    "{word:?} was lost from {source:?} at width {width}:\n{out}"
                );
            }
        }
    }
}

#[test]
fn markdown_deeply_nested_lists_flatten_rather_than_lose_their_text() {
    // Indentation is decoration and the text is the message, so a narrow frame drops the
    // indent instead of letting the prefix push the sentence off the row.
    let source = "- aa\n  - bb\n    - cc\n      - dd\n        - ee\n          - ff\n";
    let out = flattened(&draw(source, 12));
    for word in ["aa", "bb", "cc", "dd", "ee", "ff"] {
        assert!(
            out.contains(word),
            "level {word:?} lost its text to its own indentation:\n{out}"
        );
    }
}

#[test]
fn markdown_an_empty_or_blank_source_renders_nothing_at_all() {
    for source in ["", "\n", "   \n\n  \n", "\t"] {
        assert!(
            draw(source, 40).is_empty(),
            "{source:?} produced rows, so an empty reply would print a gap"
        );
    }
}

#[test]
fn markdown_a_degenerate_width_does_not_panic() {
    // Bounded by `max(width, 2)`: the two-column floor is `truncate_row`'s documented
    // single-cluster exception, which is what keeps a CJK glyph from being deleted rather
    // than drawn one column wide.
    for width in [0_u16, 1, 2, 3] {
        for row in &draw(SAMPLE, width) {
            let used = row_width(row);
            assert!(
                used <= usize::from(width).max(2),
                "a {width}-column frame emitted {used} columns: {:?}",
                row.iter()
                    .map(|span| span.content.as_ref())
                    .collect::<Vec<_>>()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Purity, so the caches in the memory plan can sit above this
// ---------------------------------------------------------------------------

#[test]
fn markdown_render_is_a_pure_function_of_source_width_and_palette() {
    // `.omo/plans/memory-perf-optimization.md` §3.3 R2-R5 memoise this function. A
    // memoised function that consults hidden state returns the wrong answer, so equal
    // inputs must give equal output and a changed palette must give a different one.
    let dark = palette();
    let light = ThemeRegistry::new()
        .resolve(crate::theme::DEFAULT_THEME, Mode::Light)
        .palette;
    let first = render(SAMPLE, 72, &dark);
    assert_eq!(render(SAMPLE, 72, &dark), first, "two calls disagreed");
    // Order must not matter either: rendering something else in between cannot change it.
    let _ = render("something else entirely\n", 31, &light);
    assert_eq!(render(SAMPLE, 72, &dark), first, "an unrelated call leaked");
    assert_ne!(
        render(SAMPLE, 72, &light),
        first,
        "the palette is not reaching the output, so a cache keyed on it would serve \
         yesterday's colours after a theme switch"
    );
    assert_ne!(
        render(SAMPLE, 40, &dark),
        first,
        "the width is not reaching"
    );
}

// ---------------------------------------------------------------------------
// The frame: these rows reach cells, through the real transcript
// ---------------------------------------------------------------------------

#[test]
fn markdown_reaches_the_transcript_frame_at_every_width_the_plan_names() {
    // §11.6's widths. Asserted on the frame so this is not a claim about an
    // intermediate row vector: these are the cells a user would be looking at.
    for width in [200_u16, 120, 80, 60, 40] {
        let drawn = frame(SAMPLE, width, 40);
        let out = drawn.join("\n");
        assert!(
            out.contains("Assistant"),
            "the transcript did not render at {width}:\n{out}"
        );
        // The row carrying the bullet is located by content, so this cannot pass by
        // asserting against a row something else is covering.
        let bullet = drawn
            .iter()
            .find(|row| row.contains('•'))
            .unwrap_or_else(|| panic!("no bullet on screen at {width}:\n{out}"));
        assert!(
            bullet.contains("views/diff.rs"),
            "the bullet row is not the list item at {width}: {bullet:?}"
        );
        assert!(
            drawn.iter().any(|row| row.contains("╭─")),
            "no code frame on screen at {width}:\n{out}"
        );
        assert!(
            !out.contains("**") && !out.contains("```"),
            "raw markup reached the cells at {width}:\n{out}"
        );
        // The column property is asserted on rows, not here: `testkit::rows` yields one
        // entry per *cell*, so a wide glyph arrives as the glyph plus the blank
        // continuation cell the terminal reserved, and `display_width` over that string
        // double-counts every CJK character. `message_tests::line_columns` documents the
        // same trap. What a frame can prove is that the content is present and legible,
        // which is what the assertions above do.
        // One character, not the phrase: `rows` emits the terminal's blank continuation
        // cell after each wide glyph, so the cells read `中 文 说 明` and no contiguous
        // substring of the source can match. Asserting one glyph is the honest form.
        assert!(
            drawn.iter().any(|row| row.contains('中')),
            "the CJK list item is not on screen at {width}"
        );
    }
}

#[test]
fn markdown_survives_the_degenerate_frame() {
    // §11.6: 20×10 must not panic. It cannot show much, and what it does show must still
    // be the reply rather than punctuation.
    let drawn = frame(SAMPLE, 20, 10);
    assert_eq!(drawn.len(), 10);
    let out = drawn.join("\n");
    assert!(
        out.contains("Assistant"),
        "the role header was lost at 20×10:\n{out}"
    );
    let widest = drawn
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    assert!(
        widest <= 20,
        "a row occupies {widest} cells, so it escaped the 20-column frame"
    );
    // And the smallest frames the backend will take, for the panic property alone.
    let _ = frame(SAMPLE, 1, 1);
    let _ = frame("```\n中\n", 2, 2);
}

#[test]
fn markdown_leaves_the_left_rule_and_the_role_header_alone() {
    // §11.2 keeps three designs: the rule runs down every row, the header prints only on
    // a change of speaker, and the roles differ by colour. Routing prose through the
    // markdown renderer must not disturb any of them.
    let mut view = TranscriptView::new(ViewContext::defaults());
    view.transcript_mut()
        .push(Message::user("**not** reinterpreted"));
    for _ in 0..2 {
        let mut message = Message::new(Role::Assistant);
        message.parts.push(MessagePart::Text {
            text: String::from("# Step\n\n- did a thing\n"),
        });
        view.transcript_mut().push(message);
    }
    let drawn = rows(&render_offscreen(&mut view, 48, 24).expect("infallible"));
    let out = drawn.join("\n");
    assert_eq!(
        out.matches("Assistant").count(),
        1,
        "a two-step turn printed its header twice:\n{out}"
    );
    assert!(
        out.contains("**not** reinterpreted"),
        "the user's own asterisks were rewritten, so the surface edited their input:\n{out}"
    );
    // `Role::marker` draws the user's turn with `▌` and the assistant's with `│`, and the
    // rule runs down every row of a turn rather than only its header.
    let user = drawn.iter().filter(|row| row.starts_with('▌')).count();
    let assistant = drawn.iter().filter(|row| row.starts_with('│')).count();
    assert!(
        user >= 2,
        "the user's rule is missing ({user} rows):\n{out}"
    );
    assert!(
        assistant >= 6,
        "the assistant's rule does not run down the turn ({assistant} rows):\n{out}"
    );
    let bullet = drawn
        .iter()
        .find(|row| row.contains('•'))
        .expect("the assistant's list did not render");
    assert!(
        bullet.starts_with('│'),
        "a markdown row escaped the role gutter: {bullet:?}"
    );
}

#[test]
fn markdown_is_visible_on_the_row_the_assertion_reads() {
    // The recorded lesson, made into a test: an assertion that only checks a row index
    // can pass while something covers the row. This locates the heading's own row, then
    // proves the cells on it are the heading and that the `#` is gone.
    let drawn = frame("# Findings\n\nthe body\n", 40, 8);
    let heading = drawn
        .iter()
        .position(|row| row.contains("Findings"))
        .expect("the heading is not on screen at all");
    let body = drawn
        .iter()
        .position(|row| row.contains("the body"))
        .expect("the body is not on screen at all");
    assert!(heading < body, "the heading rendered below its paragraph");
    assert!(
        !drawn[heading].contains('#'),
        "the heading row still carries its hashes: {:?}",
        drawn[heading]
    );
}

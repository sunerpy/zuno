//! Command argument expansion, specified from Zuno's own contract.
//!
//! `expand`, `hints`, and `tokenize` in `zuno_catalog::command` implement the
//! slash-command argument macros documented in `docs/config/workflows.md`:
//!
//! - `$1`, `$2`, `$3` and so on expand to that single tokenized argument;
//! - the highest-numbered placeholder is greedy: it takes every remaining
//!   argument, joined by one space;
//! - a placeholder past the end of the argument list expands to nothing;
//! - `$ARGUMENTS` expands to the raw, untokenized input;
//! - a template that mentions no placeholder at all has the raw input appended
//!   after a blank line, unless the input is blank;
//! - the result is trimmed, and expansion never fails or panics.
//!
//! Every expected value below is written from that contract and from the Rust
//! implementation's own documentation. Two further rules the contract implies are
//! specified here as well: a placeholder that names no argument expands to
//! nothing, and a substituted argument is data rather than template syntax, so
//! expansion never reads back what it just wrote.

use zuno_catalog::command::{expand, hints, tokenize};

/// Run `expand` over a table of `(template, arguments, expected)` rows and
/// report every divergence at once, so one failing row does not hide the rest.
fn assert_expands(rows: &[(&str, &str, &str)]) {
    let failures: Vec<String> = rows
        .iter()
        .filter_map(|(template, arguments, expected)| {
            let got = expand(template, arguments);
            (got != *expected).then(|| {
                format!(
                    "template  {template:?}\n  arguments {arguments:?}\n  expected  {expected:?}\n  got       {got:?}"
                )
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} expansions diverged from the contract:\n\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n\n")
    );
}

/// Run `tokenize` over a table of `(arguments, expected tokens)` rows.
fn assert_tokenizes(rows: &[(&str, &[&str])]) {
    for (arguments, expected) in rows {
        assert_eq!(
            tokenize(arguments),
            *expected,
            "tokenizing {arguments:?} should follow the documented token grammar"
        );
    }
}

/// Run `hints` over a table of `(template, expected hints)` rows.
fn assert_hints(rows: &[(&str, &[&str])]) {
    for (template, expected) in rows {
        assert_eq!(
            hints(template),
            *expected,
            "hints for {template:?} should list each placeholder once, sorted, with $ARGUMENTS last"
        );
    }
}

// ---------------------------------------------------------------------------
// `$ARGUMENTS`
// ---------------------------------------------------------------------------

#[test]
fn arguments_placeholder_receives_the_raw_input_at_every_occurrence() {
    assert_expands(&[
        ("Input: $ARGUMENTS", "extra args", "Input: extra args"),
        // Every occurrence is substituted, not just the first.
        (
            "Input: $ARGUMENTS\n\nRun: `git show $ARGUMENTS`",
            "abc123",
            "Input: abc123\n\nRun: `git show abc123`",
        ),
        // The placeholder is matched inside a word and is case-sensitive.
        ("x$ARGUMENTSy", "MID", "xMIDy"),
        ("$arguments and $ARGUMENTS", "up", "$arguments and up"),
        // Quotes, repeated spaces, and newlines survive because this is the
        // untokenized input.
        ("$ARGUMENTS", "\"a b\"", "\"a b\""),
        (
            "P=[$1] ALL=[$ARGUMENTS]",
            "\"quoted arg\"  spaced",
            "P=[quoted arg spaced] ALL=[\"quoted arg\"  spaced]",
        ),
        (
            "A=[$1] ALL=[$ARGUMENTS]",
            "one\ntwo",
            "A=[one two] ALL=[one\ntwo]",
        ),
        (
            "$ARGUMENTS",
            "h\u{e9}llo w\u{f6}rld",
            "h\u{e9}llo w\u{f6}rld",
        ),
        // `$ARGUMENTS` and positionals are independent views of the same input,
        // whichever order the template mentions them in.
        ("$ARGUMENTS then $1", "a b", "a b then a b"),
        (
            "A=[$1] B=[$2] C=[$3] ALL=[$ARGUMENTS]",
            "one two",
            "A=[one] B=[two] C=[] ALL=[one two]",
        ),
    ]);
}

#[test]
fn arguments_placeholder_with_blank_input_expands_to_nothing() {
    assert_expands(&[
        ("Input: $ARGUMENTS", "", "Input:"),
        // Whitespace-only input substitutes as-is; the final trim removes it.
        ("Input: $ARGUMENTS", "   ", "Input:"),
    ]);
}

// ---------------------------------------------------------------------------
// Positional placeholders
// ---------------------------------------------------------------------------

#[test]
fn positional_placeholders_take_one_token_each_and_the_highest_is_greedy() {
    assert_expands(&[
        (
            "A=[$1] B=[$2]",
            "one two three four",
            "A=[one] B=[two three four]",
        ),
        ("only: $1", "one two three", "only: one two three"),
        // Greediness follows the highest number, not the last position in the
        // text.
        ("B=[$2] A=[$1]", "one two three", "B=[two three] A=[one]"),
        // A repeated placeholder receives the same value each time.
        ("$1 and $1 and $2", "x y z", "x and x and y z"),
        // A gap in the numbering does not shift the arguments.
        (
            "A=[$1] C=[$3]",
            "one two three four",
            "A=[one] C=[three four]",
        ),
        // Two-digit placeholders are read as one number.
        (
            "T=[$10] ONE=[$1]",
            "a b c d e f g h i j k",
            "T=[j k] ONE=[a]",
        ),
        ("$2 and $10", "a b c d e f g h i j k l", "b and j k l"),
        // A leading zero is still that number.
        ("A=[$01] B=[$2]", "one two three", "A=[one] B=[two three]"),
        // Placeholders need no delimiter on either side.
        ("pre$1post", "VAL", "preVALpost"),
        ("$1$2$3", "a b c d", "abc d"),
        ("$1a $2b", "x y z", "xa y zb"),
        (
            "line1 $1\nline2 $2\nline3",
            "a b c",
            "line1 a\nline2 b c\nline3",
        ),
    ]);
}

#[test]
fn a_positional_past_the_end_of_the_input_expands_to_nothing() {
    assert_expands(&[
        ("A=[$1] B=[$2] C=[$3]", "one two", "A=[one] B=[two] C=[]"),
        ("$1|$2|$3|$4|$5", "a b", "a|b|||"),
        ("$999", "a b", ""),
        // A number too large for the implementation's integer type saturates,
        // which is unobservable: it is past the end like any other.
        ("$99999999999999999999", "a b", ""),
    ]);
}

#[test]
fn any_dollar_and_ascii_digits_is_a_placeholder() {
    assert_expands(&[
        // A consequence of the contract worth knowing: a price in a template
        // is read as `$5` followed by `.00`, and `$5` is past the end here.
        (
            "COST IS $5.00 and $x and $",
            "one two",
            "COST IS .00 and $x and $",
        ),
        ("COST IS $5.00 and $x and $", "", "COST IS .00 and $x and $"),
        // Only ASCII digits form a placeholder.
        ("$\u{663} and $1", "a b", "$\u{663} and a b"),
    ]);
}

#[test]
fn positionals_are_fed_by_the_tokenizer() {
    assert_expands(&[
        (
            "A=[$1] B=[$2]",
            "\"hello world\" second",
            "A=[hello world] B=[second]",
        ),
        (
            "A=[$1] B=[$2]",
            "'hello world' second",
            "A=[hello world] B=[second]",
        ),
        (
            "A=[$1] B=[$2] C=[$3]",
            "one    two\tthree",
            "A=[one] B=[two] C=[three]",
        ),
        (
            "A=[$1] B=[$2]",
            "[Image 3] caption",
            "A=[[Image 3]] B=[caption]",
        ),
        ("A=[$1]", "[Image 12]", "A=[[Image 12]]"),
        // An empty quoted run is an empty token, so `$1` is present but empty.
        ("A=[$1] B=[$2]", "\"\" second", "A=[] B=[second]"),
        // An unpaired quote is not a token at all, so the arguments shift.
        ("A=[$1] B=[$2]", "\" second", "A=[second] B=[]"),
        ("A=[$1] B=[$2]", "don't stop", "A=[don] B=[t stop]"),
        (
            "A=[$1] B=[$2]",
            "\u{65e5}\u{672c}\u{8a9e} \u{30c6}\u{30b9}\u{30c8} \u{4e09}\u{3064}",
            "A=[\u{65e5}\u{672c}\u{8a9e}] B=[\u{30c6}\u{30b9}\u{30c8} \u{4e09}\u{3064}]",
        ),
    ]);
}

// ---------------------------------------------------------------------------
// The no-placeholder fallback and trimming
// ---------------------------------------------------------------------------

#[test]
fn a_template_without_placeholders_has_the_raw_input_appended() {
    assert_expands(&[
        (
            "NO PLACEHOLDERS HERE",
            "trailing input",
            "NO PLACEHOLDERS HERE\n\ntrailing input",
        ),
        // The appended text is the raw input, not the tokens.
        (
            "NO PLACEHOLDERS HERE",
            "\"quoted\"  spaced",
            "NO PLACEHOLDERS HERE\n\n\"quoted\"  spaced",
        ),
        // Blank input is not appended.
        ("NO PLACEHOLDERS HERE", "", "NO PLACEHOLDERS HERE"),
        ("NO PLACEHOLDERS HERE", "  \t ", "NO PLACEHOLDERS HERE"),
        // An empty template still receives the input.
        ("", "just this", "just this"),
        ("", "", ""),
        // Any placeholder, positional or `$ARGUMENTS`, disables the fallback.
        ("A=[$1]", "one two", "A=[one two]"),
        ("A=[$ARGUMENTS]", "one two", "A=[one two]"),
    ]);
}

#[test]
fn expansion_trims_the_result() {
    assert_expands(&[("   $ARGUMENTS   ", "mid", "mid"), ("   $1   ", "", "")]);
}

// ---------------------------------------------------------------------------
// `hints`
// ---------------------------------------------------------------------------

#[test]
fn hints_list_each_placeholder_once_sorted_lexicographically_with_arguments_last() {
    assert_hints(&[
        ("$1 and $1 and $2", &["$1", "$2"]),
        // The sort is on the text, so `$10` precedes `$2`.
        ("$2 and $10", &["$10", "$2"]),
        ("T=[$10] ONE=[$1]", &["$1", "$10"]),
        // The raw spelling survives.
        ("$01 and $1", &["$01", "$1"]),
        ("$99999999999999999999", &["$99999999999999999999"]),
        // `$ARGUMENTS` is appended after the sorted positionals wherever it
        // appears in the template, and is case-sensitive.
        ("A=[$1] ALL=[$ARGUMENTS]", &["$1", "$ARGUMENTS"]),
        ("$ARGUMENTS then $1", &["$1", "$ARGUMENTS"]),
        ("Input: $ARGUMENTS", &["$ARGUMENTS"]),
        ("$arguments and $ARGUMENTS", &["$ARGUMENTS"]),
        // The same scanner as `expand`: ASCII digits only, and a price counts.
        ("COST IS $5.00 and $x and $", &["$5"]),
        ("$\u{663} and $1", &["$1"]),
        ("nothing here", &[]),
        ("", &[]),
    ]);
}

// ---------------------------------------------------------------------------
// `tokenize`
// ---------------------------------------------------------------------------

#[test]
fn tokenize_splits_on_whitespace_and_keeps_quoted_runs_and_image_markers_whole() {
    assert_tokenizes(&[
        ("one two", &["one", "two"]),
        ("one    two\tthree", &["one", "two", "three"]),
        ("one\ntwo", &["one", "two"]),
        ("", &[]),
        ("   ", &[]),
        // A quoted run is one token and loses its quotes; either quote works.
        ("\"hello world\" second", &["hello world", "second"]),
        ("'hello world' second", &["hello world", "second"]),
        ("\"\" second", &["", "second"]),
        // A rendered image marker is one token, case-insensitively.
        ("[Image 3] caption", &["[Image 3]", "caption"]),
        ("[image 12] tail", &["[image 12]", "tail"]),
        ("[Image 12]", &["[Image 12]"]),
        // Non-ASCII text is split on whitespace like anything else.
        (
            "\u{65e5}\u{672c}\u{8a9e} \u{30c6}",
            &["\u{65e5}\u{672c}\u{8a9e}", "\u{30c6}"],
        ),
        // Dollar signs are ordinary characters to the tokenizer.
        ("$$ tail", &["$$", "tail"]),
        ("$& tail", &["$&", "tail"]),
        ("x $` y", &["x", "$`", "y"]),
    ]);
}

#[test]
fn tokenize_skips_an_unpaired_quote_and_splits_at_an_apostrophe() {
    assert_tokenizes(&[
        // An unpaired quote matches nothing and is skipped, so the run after it
        // becomes the token.
        ("\" second", &["second"]),
        ("\"unclosed second", &["unclosed", "second"]),
        // An apostrophe opens a quoted run that never closes, so `don't` is
        // two tokens and the apostrophe itself vanishes.
        ("don't stop", &["don", "t", "stop"]),
        ("x $' y", &["x", "$", "y"]),
    ]);
}

#[test]
fn an_image_marker_needs_a_space_digits_and_a_closing_bracket() {
    assert_tokenizes(&[
        ("[Image] x", &["[Image]", "x"]),
        ("[Image3] x", &["[Image3]", "x"]),
        ("[Image 3 x", &["[Image", "3", "x"]),
    ]);
}

// ---------------------------------------------------------------------------
// Placeholders that name no argument
// ---------------------------------------------------------------------------

/// `$0` names no argument, because the documented placeholders start at `$1`.
///
/// It therefore expands to nothing, exactly like any position past the end of
/// the argument list. Earlier releases rendered it as the literal text
/// `undefined`, or as the last argument when `$0` was the highest placeholder in
/// the template; both were artefacts of negative array indexing in the
/// JavaScript the implementation was first transcribed from, and neither was
/// ever part of Zuno's contract.
#[test]
fn dollar_zero_expands_to_nothing_like_any_out_of_range_position() {
    assert_expands(&[
        ("Z=[$0] ONE=[$1]", "one two", "Z=[] ONE=[one two]"),
        ("Z=[$00] ONE=[$1]", "one two", "Z=[] ONE=[one two]"),
        ("only: $0", "a b c", "only:"),
        ("only: $0", "", "only:"),
        // A high out-of-range placeholder does take the greedy highest slot, so
        // `$1` stops being greedy and the remaining arguments expand nowhere.
        // `$0` never does, because zero is below every real position.
        ("Z=[$999] ONE=[$1]", "one two", "Z=[] ONE=[one]"),
        ("Z=[$0] ONE=[$1]", "one two", "Z=[] ONE=[one two]"),
    ]);
    // `hints` reports the template's placeholder inventory rather than deciding
    // what resolves, so an out-of-range placeholder is still offered.
    assert_hints(&[
        ("Z=[$0] ONE=[$1]", &["$0", "$1"]),
        ("Z=[$00] ONE=[$1]", &["$00", "$1"]),
    ]);
}

/// `$ARGUMENTS` inserts the input verbatim, as the documented contract says.
///
/// Every byte the user typed survives: `$$` stays `$$`, `$&` stays `$&`, and
/// `` $` `` and `$'` cannot pull the surrounding template text into the
/// substitution. Earlier releases performed this substitution through
/// JavaScript's replacement-pattern machinery, which interpreted those sequences
/// *inside the user's own input*, so someone passing a shell snippet containing
/// `$$` did not get what they typed.
#[test]
fn arguments_inserts_the_input_verbatim() {
    assert_expands(&[
        ("ALL=[$ARGUMENTS]", "cost $$ high", "ALL=[cost $$ high]"),
        ("ALL=[$ARGUMENTS]", "$& weird", "ALL=[$& weird]"),
        (
            "pre ALL=[$ARGUMENTS] post",
            "x $` y",
            "pre ALL=[x $` y] post",
        ),
        (
            "pre ALL=[$ARGUMENTS] post",
            "x $' y",
            "pre ALL=[x $' y] post",
        ),
        // Positional substitution is literal in the same way.
        ("P=[$1]", "$$ tail", "P=[$$ tail]"),
        ("P=[$1] Q=[$2]", "$& tail", "P=[$&] Q=[tail]"),
    ]);
}

/// A substituted argument is data, not template syntax.
///
/// Expansion makes one left-to-right pass and never reads back what it wrote, so
/// an argument that happens to contain `$ARGUMENTS` or `$2` is inserted as text.
/// Rescanning would let the input duplicate itself: `$1` with the input
/// `$ARGUMENTS x` used to expand to `$ARGUMENTS x x`.
#[test]
fn a_substituted_argument_is_never_expanded_again() {
    assert_expands(&[
        ("$1", "$ARGUMENTS x", "$ARGUMENTS x"),
        ("A=[$1]", "$ARGUMENTS tail", "A=[$ARGUMENTS tail]"),
        (
            "A=[$1] B=[$2]",
            "$ARGUMENTS tail",
            "A=[$ARGUMENTS] B=[tail]",
        ),
        ("A=[$ARGUMENTS]", "$1 $2", "A=[$1 $2]"),
    ]);
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// A template can be adversarial without becoming a panic.
///
/// `expand` is documented to never fail, never panic, and always return a
/// trimmed string. Every input below is a string a user could plausibly type
/// into a command template or as its arguments.
#[test]
fn command_template_input_never_panics() {
    let templates = [
        "",
        "$",
        "$$",
        "$$$",
        "$0",
        "$00000",
        "$1$2$3$4$5$6$7$8$9$10",
        "$99999999999999999999999999999999",
        "$ARGUMENTS$ARGUMENTS",
        "$ARGUMENT",
        "\u{65e5}\u{672c}$1\u{8a9e}",
        "$\u{663}",
        "${path}",
        "!`echo hi` $1",
        "\0$1\0",
        "$1\u{feff}$2",
    ];
    let arguments = [
        "",
        " ",
        "\t\n",
        "one",
        "one two",
        "\"unclosed",
        "'unclosed",
        "\"\"",
        "''",
        "$$ $& $` $'",
        "[Image 1] [Image 2]",
        "[Image]",
        "\u{65e5}\u{672c}\u{8a9e}",
        "\0",
        "a\u{feff}b",
        &"x ".repeat(64),
    ];

    for template in templates {
        let _ = hints(template);
        for argument in arguments {
            let _ = tokenize(argument);
            let expanded = expand(template, argument);
            assert_eq!(
                expanded,
                expanded.trim(),
                "expansion always trims: {template:?} + {argument:?}"
            );
        }
    }
}

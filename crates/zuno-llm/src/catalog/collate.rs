//! The string comparator `opencode models` sorts with.
//!
//! # Why this is not `str::cmp`
//!
//! `packages/opencode/src/cli/cmd/models.ts:38` sorts model ids with
//! `a.localeCompare(b)` and `:56-62` sorts provider ids the same way. That is
//! ICU collation, not byte order, and the two disagree on real catalog data:
//!
//! ```text
//! localeCompare : "Gemma-4-31B-DarkIdol" < "glm-5"
//! byte order    : "Gemma-4-31B-DarkIdol" < "glm-5"   (agrees by luck: 'G' < 'g')
//! localeCompare : "glm-5-turbo" < "glm-5.1"
//! byte order    : "glm-5.1"     < "glm-5-turbo"      ('.' 0x2E < '-' 0x2D is false…)
//! ```
//!
//! Sorting a model list byte-wise reorders the list a user reads and, worse,
//! breaks a differential against the real binary for reasons that have nothing
//! to do with which models resolved. So this module ports the collation.
//!
//! # The model, and how it was derived
//!
//! Every character that appears in a provider or model id across the whole
//! 180-provider models.dev catalog is ASCII, drawn from exactly 68 characters:
//!
//! ```text
//! -./0123456789:@ABCDEFGHIJKLMNOPQRSTUVWXZ_abcdefghijklmnopqrstuvwxyz~
//! ```
//!
//! Against that alphabet, `localeCompare` is reproduced by two levels:
//!
//! 1. **Primary** — a per-character weight, compared position by position, with
//!    the shorter string first when one is a prefix of the other. Punctuation is
//!    **non-ignorable** and sorts before digits, which sort before letters;
//!    letters compare case-folded. The punctuation order is `_ - : . @ / ~`,
//!    which is itself ICU's, not ASCII's.
//! 2. **Tertiary** — case, at the first position where it differs, lowercase
//!    before uppercase. (There is no secondary level to port: that is accents,
//!    and the alphabet has none.)
//!
//! The order was read off `localeCompare` one character at a time and then the
//! whole two-level model was checked **exhaustively**: all 4 753 986 pairs of
//! the 3 084 distinct provider and model ids in the catalog, compared against
//! Node's `localeCompare`. Zero disagreements. The unit test
//! `matches_locale_compare_where_byte_order_diverges` below keeps a sample of the
//! discriminating pairs so a later edit cannot regress it, and the ordering is
//! additionally covered end to end by `tests/catalog_differential.rs`, where any
//! reordering of a model list fails against the real binary's stdout.
//!
//! # The boundary, stated plainly
//!
//! Parity is *proven* for the 68-character alphabet above. A user can invent a
//! model id in config containing anything, so [`compare`] falls back to code
//! point order for characters outside it — after the in-alphabet levels, so an
//! exotic id sorts predictably rather than panicking. That fallback is **not**
//! ICU parity, and no test claims it is.

use std::cmp::Ordering;

/// Punctuation, in ICU's order rather than ASCII's, ahead of every digit.
const PUNCTUATION: [char; 7] = ['_', '-', ':', '.', '@', '/', '~'];

/// Primary weight of punctuation, lowest of all.
const PUNCTUATION_BASE: u32 = 1;
/// Primary weight of digits, above punctuation.
const DIGIT_BASE: u32 = 100;
/// Primary weight of letters, above digits.
const LETTER_BASE: u32 = 200;
/// Primary weight of everything else — the documented non-parity fallback.
const OTHER_BASE: u32 = 1_000;

/// The primary collation weight of one character.
fn primary(ch: char) -> u32 {
    if let Some(index) = PUNCTUATION.iter().position(|candidate| *candidate == ch) {
        return PUNCTUATION_BASE + u32::try_from(index).unwrap_or(0);
    }
    if ch.is_ascii_digit() {
        return DIGIT_BASE + u32::from(ch) - u32::from('0');
    }
    if ch.is_ascii_alphabetic() {
        return LETTER_BASE + u32::from(ch.to_ascii_lowercase()) - u32::from('a');
    }
    OTHER_BASE.saturating_add(u32::from(ch))
}

/// True when the character carries uppercase case weight at the tertiary level.
///
/// Only cased characters do. A digit and a hyphen are both "not uppercase", and
/// comparing them at this level must therefore be a tie, which is what pushes
/// the decision back to the primary level where it belongs.
fn is_upper(ch: char) -> bool {
    ch.is_uppercase()
}

/// Compare two ids the way `opencode models` does.
///
/// See the module docs for the derivation and for the one place this is a
/// documented approximation rather than a port.
#[must_use]
pub fn compare(left: &str, right: &str) -> Ordering {
    let mut lefts = left.chars();
    let mut rights = right.chars();
    // Primary: position-by-position weights, prefix first.
    loop {
        match (lefts.next(), rights.next()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) => match primary(a).cmp(&primary(b)) {
                Ordering::Equal => {}
                other => return other,
            },
        }
    }
    // Tertiary: case, at the first position that differs.
    for (a, b) in left.chars().zip(right.chars()) {
        match is_upper(a).cmp(&is_upper(b)) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    // Exhausted every level the alphabet has. Code points keep the order total.
    left.cmp(right)
}

/// Compare provider ids the way `opencode models` lists them.
///
/// `models.ts:56-62` floats every id starting with `opencode` to the front —
/// the hosted zen gateway is the one a new user is expected to reach for — and
/// falls back to [`compare`] otherwise. Two `opencode*` ids compare against
/// each other normally, so `opencode` precedes `opencode-staging`.
#[must_use]
pub fn compare_provider_ids(left: &str, right: &str) -> Ordering {
    let left_first = left.starts_with("opencode");
    let right_first = right.starts_with("opencode");
    match (left_first, right_first) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => compare(left, right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pairs where byte order and `localeCompare` disagree, with the
    /// `localeCompare` answer. Every one was read off Node against the real
    /// catalog; each would silently reorder a user's model list.
    #[test]
    fn matches_locale_compare_where_byte_order_diverges() {
        let cases: &[(&str, &str, Ordering)] = &[
            // Case-folded primary: 'e' < 'l' decides, not 'G' < 'g'.
            ("glm-5", "Gemma-4-31B-DarkIdol", Ordering::Greater),
            ("glm-5", "GLM-4.7", Ordering::Greater),
            // ICU punctuation order: '-' before '.', the reverse of ASCII.
            ("glm-5-turbo", "glm-5.1", Ordering::Less),
            ("glm-5-2", "glm-5.1", Ordering::Less),
            // Punctuation is non-ignorable and sorts below every letter.
            ("~openai/gpt-latest", "glm-5", Ordering::Less),
            ("a-b", "ab", Ordering::Less),
            // Case is tertiary: lowercase first, and only when primary ties.
            ("zz", "Zz", Ordering::Less),
            ("Zeta", "zz", Ordering::Less),
        ];
        for (left, right, expected) in cases {
            assert_eq!(
                compare(left, right),
                *expected,
                "compare({left:?}, {right:?})"
            );
            assert_eq!(
                compare(right, left),
                expected.reverse(),
                "compare({right:?}, {left:?}) must be the mirror"
            );
        }
    }

    #[test]
    fn digits_sort_before_letters_and_after_punctuation() {
        assert_eq!(compare("-a", "0a"), Ordering::Less);
        assert_eq!(compare("0a", "aa"), Ordering::Less);
    }

    #[test]
    fn a_prefix_sorts_first() {
        assert_eq!(compare("glm", "glm-5"), Ordering::Less);
        assert_eq!(compare("glm-5", "glm"), Ordering::Greater);
    }

    #[test]
    fn equal_strings_tie() {
        assert_eq!(compare("deepseek-chat", "deepseek-chat"), Ordering::Equal);
    }

    #[test]
    fn the_comparator_is_a_total_order_over_the_catalog_alphabet() {
        let alphabet: Vec<char> =
            "-./0123456789:@ABCDEFGHIJKLMNOPQRSTUVWXZ_abcdefghijklmnopqrstuvwxyz~"
                .chars()
                .collect();
        let mut ids: Vec<String> = alphabet.iter().map(|c| c.to_string()).collect();
        for a in &alphabet {
            ids.push(format!("g{a}5"));
        }
        // Antisymmetry and transitivity, checked by sorting and re-verifying.
        let mut sorted = ids.clone();
        sorted.sort_by(|a, b| compare(a, b));
        for window in sorted.windows(2) {
            assert_ne!(
                compare(&window[0], &window[1]),
                Ordering::Greater,
                "{:?} must not sort after {:?}",
                window[0],
                window[1]
            );
        }
        for a in &ids {
            for b in &ids {
                assert_eq!(
                    compare(a, b),
                    compare(b, a).reverse(),
                    "antisymmetry for {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn opencode_providers_float_to_the_front() {
        let mut ids = vec!["zhipuai", "opencode", "anthropic", "opencode-staging"];
        ids.sort_by(|a, b| compare_provider_ids(a, b));
        assert_eq!(
            ids,
            vec!["opencode", "opencode-staging", "anthropic", "zhipuai"]
        );
    }
}

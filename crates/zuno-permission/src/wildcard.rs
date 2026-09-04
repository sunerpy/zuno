use crate::types::Rule;
use zuno_config::schema::permission::PermissionAction;

/// Match a whole string using the oracle's wildcard syntax.
///
/// `*` matches zero or more UTF-16 code units, `?` matches exactly one, and every
/// other character is literal — on every platform. This is the **identity**
/// comparison, so it neither folds case nor reads `\` as `/`: both of those relate
/// two spellings that can name different things (a file in a case-sensitive
/// directory, a POSIX file name that contains `\`, a `\` the shell removes), and a
/// grant has to keep naming exactly what the user named. Where one of those readings
/// is identity on the host — separators in a path on Windows — [`crate::resource`]
/// supplies the spelling; where it is not, [`wildcard_match_folded`] offers it to a
/// `deny` alone, which may over-refuse.
///
/// A trailing `" *"` is special: it accepts the command with no arguments as well
/// as a space followed by arguments.
#[must_use]
pub fn wildcard_match(input: &str, pattern: &str) -> bool {
    if let Some(command) = pattern.strip_suffix(" *")
        && matches_units(input, command)
    {
        return true;
    }
    matches_units(input, pattern)
}

/// [`wildcard_match`] with case folded and `\` read as `/` on both sides.
///
/// Only a `deny` may use this. It lets a prohibition hold on a case-insensitive
/// volume — macOS by default, Windows almost always — where `RM -rf /` runs
/// `/bin/rm`, and wherever a path or an argument is written with the other
/// separator. Applied to a grant it would let allow `src/a/b` cover the distinct
/// POSIX file `src/a\b`, or `Secret.rs` in a case-sensitive NTFS directory.
#[must_use]
pub(crate) fn wildcard_match_folded(input: &str, pattern: &str) -> bool {
    wildcard_match(&fold(input), &fold(pattern))
}

/// The deny-side reading of one spelling: case folded and `\` read as `/`.
pub(crate) fn fold(value: &str) -> String {
    value.replace('\\', "/").to_lowercase()
}

/// Whether `rule` is written for the permission `key`.
///
/// The key comparison is literal, like every other identity comparison; a `deny`
/// is also matched with case folded, so a prohibition written `Shell` still holds on
/// every platform while a grant written that way governs nothing.
#[must_use]
pub(crate) fn key_governs(key: &str, rule: &Rule) -> bool {
    wildcard_match(key, &rule.permission)
        || (rule.action == PermissionAction::Deny && wildcard_match_folded(key, &rule.permission))
}

fn matches_units(input: &str, pattern: &str) -> bool {
    const STAR: u16 = b'*' as u16;
    const QUESTION: u16 = b'?' as u16;

    let input: Vec<_> = input.encode_utf16().collect();
    let pattern: Vec<_> = pattern.encode_utf16().collect();
    let mut input_index = 0;
    let mut pattern_index = 0;
    let mut star_index = None;
    let mut star_input_index = 0;

    // `*` is tested before the literal comparison on purpose. A `*` in the input is
    // an ordinary character, so the literal branch would otherwise consume the
    // pattern's star as a literal star and `rm *.txt` would escape `"rm *": "deny"`.
    while input_index < input.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == STAR {
            star_index = Some(pattern_index);
            star_input_index = input_index;
            pattern_index += 1;
        } else if pattern_index < pattern.len()
            && (pattern[pattern_index] == QUESTION || pattern[pattern_index] == input[input_index])
        {
            input_index += 1;
            pattern_index += 1;
        } else if let Some(star) = star_index {
            star_input_index += 1;
            input_index = star_input_index;
            pattern_index = star + 1;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == STAR {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

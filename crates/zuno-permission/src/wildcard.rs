/// Match a whole string using the oracle's wildcard syntax.
///
/// `*` matches zero or more UTF-16 code units, `?` matches exactly one, and all
/// other characters are literal. Backslashes are normalized to forward slashes.
/// A trailing `" *"` is special: it accepts the command with no arguments as
/// well as a space followed by arguments. Matching is case-insensitive only on
/// Windows.
#[must_use]
pub fn wildcard_match(input: &str, pattern: &str) -> bool {
    let input = normalize(input);
    let pattern = normalize(pattern);
    if let Some(command) = pattern.strip_suffix(" *")
        && matches_units(&input, command)
    {
        return true;
    }
    matches_units(&input, &pattern)
}

#[cfg(not(windows))]
fn normalize(value: &str) -> String {
    value.replace('\\', "/")
}

#[cfg(windows)]
fn normalize(value: &str) -> String {
    value.replace('\\', "/").to_lowercase()
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

use proptest::prelude::*;
use zuno_permission::wildcard_match;

#[test]
fn question_mark_matches_exactly_one_utf16_code_unit() {
    assert!(wildcard_match("file1.txt", "file?.txt"));
    assert!(!wildcard_match("file12.txt", "file?.txt"));
    assert!(!wildcard_match("file😀.txt", "file?.txt"));
}

#[test]
fn regex_metacharacters_are_literal() {
    assert!(wildcard_match("foo+bar.[x]", "foo+bar.[x]"));
    assert!(!wildcard_match("foooooobar.[x]", "foo+bar.[x]"));
}

#[test]
fn trailing_space_star_accepts_no_arguments_or_arguments() {
    assert!(wildcard_match("git", "git *"));
    assert!(wildcard_match("git push", "git *"));
    assert!(!wildcard_match("github", "git *"));
}

#[test]
fn star_spans_slashes_and_newlines() {
    assert!(wildcard_match("src/deep/file.rs", "src/*"));
    assert!(wildcard_match("first\nsecond", "first*second"));
}

#[test]
fn the_primitive_is_literal_on_every_platform() {
    // `wildcard_match` is the identity comparison. Reading `\` as `/` and folding
    // case both relate spellings that can name different things — a POSIX file
    // named `a\b`, a file in a case-sensitive directory — so neither happens here on
    // any platform. Where one of them is identity on the host (path separators on
    // Windows) `zuno_permission::resource` supplies the spelling, and a `deny` alone
    // is offered the folded reading there.
    assert!(!wildcard_match(
        "C:\\Windows\\System32\\drivers",
        "C:/Windows/System32/*"
    ));
    assert!(!wildcard_match(
        "C:/Windows/System32/drivers",
        "C:\\Windows\\System32\\*"
    ));
    assert!(wildcard_match(
        "C:\\Windows\\System32\\drivers",
        "C:\\Windows\\System32\\*"
    ));
    assert!(!wildcard_match("/users/test/file", "/Users/test/*"));
    assert!(wildcard_match("/Users/test/file", "/Users/test/*"));
}

#[test]
fn a_star_in_the_input_never_consumes_a_star_in_the_pattern() {
    assert!(wildcard_match("rm *.txt", "rm *"));
    assert!(wildcard_match("rm -rf x", "rm *"));
    assert!(wildcard_match("*foo", "*"));
    assert!(wildcard_match("/tmp/*x", "/tmp/*"));
}

proptest! {
    /// `*` is the pattern every catch-all rule is written with, so it must match
    /// every possible resource — including resources that contain `*` themselves.
    #[test]
    fn a_lone_star_matches_every_input(input in ".*") {
        prop_assert!(wildcard_match(&input, "*"));
    }

    #[test]
    fn a_star_matches_inputs_built_out_of_stars(stars in prop::collection::vec(Just('*'), 0..8)) {
        let input: String = stars.into_iter().collect();
        let command = format!("rm {input}");
        prop_assert!(wildcard_match(&input, "*"));
        prop_assert!(wildcard_match(&command, "rm *"));
    }
}

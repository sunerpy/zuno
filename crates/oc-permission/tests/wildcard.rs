use oc_permission::wildcard_match;

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
fn backslashes_are_normalized_in_input_and_pattern() {
    assert!(wildcard_match(
        "C:\\Windows\\System32\\drivers",
        "C:/Windows/System32/*"
    ));
    assert!(wildcard_match(
        "C:/Windows/System32/drivers",
        "C:\\Windows\\System32\\*"
    ));
}

#[test]
fn case_sensitivity_matches_the_host_platform() {
    let matches = wildcard_match("/users/test/file", "/Users/test/*");

    assert_eq!(matches, cfg!(windows));
}

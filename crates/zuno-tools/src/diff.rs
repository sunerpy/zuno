//! The unified patch a file-mutating tool reports alongside its sentence.
//!
//! # Why the patch is metadata and not the output
//!
//! The oracle's `edit.ts` computes `createTwoFilesPatch(...)` and returns it in
//! `metadata: { diff, filediff, ... }` (`tool/edit.ts:204-210`), keeping `output` the
//! one-line `"Edit applied successfully."` the model reads. Both halves matter:
//!
//! - **`output` stays a sentence.** It is what the model is charged for on every edit
//!   and what the transcript prints inline. Substituting a patch there would triple the
//!   token cost of an unremarkable edit and turn a one-line tool row into a wall of
//!   `+`/`-`.
//! - **`metadata["diff"]` is the established spelling in this workspace already.** The
//!   permission dialog reads exactly that key when it renders the change it is asking
//!   about (`zuno-tui/src/views/permission.rs`'s `edit_patch`). Producing the patch under
//!   the same key means one convention for "the patch for this change", not two.
//!
//! Consequently [`crate::read::looks_like_diff`]'s counterpart in the transcript — which
//! colours a tool result *whose output really is a patch*, such as a `git diff` shell
//! call — is untouched by this module and keeps working for that case.
//!
//! # What is not ported
//!
//! The oracle post-processes with `trimDiff`, which strips the minimum common leading
//! whitespace from every content line to save horizontal room (`edit.ts:646`). That is
//! lossy: a patch de-indented by four columns no longer shows the file's real
//! indentation, and indentation is frequently the thing an edit got wrong. The viewer
//! this feeds scrolls horizontally, so the room is not worth the lie.

use similar::TextDiff;
use std::path::Path;
use zuno_tool::FileDiff;

/// Lines of unchanged text kept on each side of a change.
///
/// Three is the `diff -u` default and what the oracle's `createTwoFilesPatch` emits, so
/// a patch produced here and a patch produced by `git diff` read the same.
pub const CONTEXT_LINES: usize = 3;

/// The `metadata` key carrying a file-mutating tool's patch.
///
/// Shared with the permission dialog's `edit_patch`, which already reads this key from a
/// permission request's metadata. One spelling, two readers.
pub const METADATA_DIFF_KEY: &str = "diff";

/// A unified diff of `old` into `new`, labelled `path` on both sides.
///
/// Both sides carry the same path because this describes an in-place modification rather
/// than a rename; the oracle labels it the same way (`createTwoFilesPatch(filePath,
/// filePath, ...)`).
///
/// Returns `None` when the two texts are equal. An empty patch is worse than no patch:
/// a viewer opened on it asserts "here is the change" while showing nothing, and the
/// caller cannot tell that from a change it failed to capture.
#[must_use]
pub fn unified_diff(path: &str, old: &str, new: &str) -> Option<String> {
    if old == new {
        return None;
    }
    let patch = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(CONTEXT_LINES)
        .header(path, path)
        .to_string();
    // `similar` emits only the header when the bodies differ in ways it folds away —
    // there is no such case for `from_lines` on unequal input, but a header-only patch
    // would parse as "no hunks" downstream, so it is filtered here rather than shipped.
    if patch.lines().any(|line| line.starts_with("@@")) {
        Some(patch)
    } else {
        None
    }
}

/// The patch between two byte sequences, when both decode as UTF-8.
///
/// File tools hold bytes, not strings: the pre-image comes off disk and the post-image is
/// re-read after the formatter runs. A binary file has no line-oriented patch, so this
/// reports `None` rather than diffing lossy replacement characters and presenting the
/// result as the change.
#[must_use]
pub fn unified_diff_bytes(path: &str, old: &[u8], new: &[u8]) -> Option<String> {
    let old = std::str::from_utf8(old).ok()?;
    let new = std::str::from_utf8(new).ok()?;
    unified_diff(path, strip_bom(old), strip_bom(new))
}

/// A native text-file diff suitable for durable client projections.
///
/// `old == None` means the file did not exist before the tool call. Binary pre/post
/// images are intentionally omitted: ACP's stable `diff` content carries text, so
/// lossy decoding would claim an edit the client cannot safely apply or display.
#[must_use]
pub fn file_diff_bytes(path: &Path, old: Option<&[u8]>, new: &[u8]) -> Option<FileDiff> {
    let old = old
        .map(std::str::from_utf8)
        .transpose()
        .ok()?
        .map(strip_bom)
        .map(str::to_owned);
    let new = strip_bom(std::str::from_utf8(new).ok()?).to_owned();
    FileDiff::new(path, old, new)
}

/// Drops a UTF-8 BOM so it does not appear as a change on the first line.
///
/// A tool preserves the pre-image's BOM, so both sides normally carry one and it cancels;
/// stripping it keeps the case where only one side does from rendering the whole first
/// line as rewritten.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_line_change_produces_a_hunk_with_both_sides() {
        let patch = unified_diff("demo.rs", "one\ntwo\nthree\n", "one\nTWO\nthree\n")
            .expect("differing texts have a patch");
        assert!(patch.contains("--- demo.rs"), "{patch}");
        assert!(patch.contains("+++ demo.rs"), "{patch}");
        assert!(patch.lines().any(|line| line.starts_with("@@")), "{patch}");
        assert!(patch.contains("-two"), "{patch}");
        assert!(patch.contains("+TWO"), "{patch}");
    }

    #[test]
    fn identical_text_has_no_patch_rather_than_an_empty_one() {
        assert_eq!(unified_diff("demo.rs", "same\n", "same\n"), None);
    }

    #[test]
    fn creating_a_file_diffs_against_nothing() {
        let patch = unified_diff("new.txt", "", "hello\n").expect("a creation is a change");
        assert!(patch.contains("+hello"), "{patch}");
    }

    #[test]
    fn context_is_bounded_so_a_large_file_does_not_become_the_patch() {
        let old = (1..=200).map(|n| format!("line {n}\n")).collect::<String>();
        let new = old.replace("line 100\n", "line ONE HUNDRED\n");
        let patch = unified_diff("big.txt", &old, &new).expect("one line differs");
        let context = patch.lines().filter(|line| line.starts_with(' ')).count();
        assert_eq!(
            context,
            CONTEXT_LINES * 2,
            "one change in the middle keeps {CONTEXT_LINES} lines each side:\n{patch}"
        );
    }

    #[test]
    fn a_binary_pre_image_reports_no_patch_instead_of_replacement_characters() {
        assert_eq!(
            unified_diff_bytes("blob.bin", &[0xff, 0xfe], b"text\n"),
            None
        );
        assert_eq!(
            file_diff_bytes(Path::new("/work/blob.bin"), Some(&[0xff, 0xfe]), b"text\n"),
            None
        );
    }

    #[test]
    fn native_file_diff_preserves_creation_vs_empty_existing_file() {
        let directory = tempfile::tempdir().expect("temporary absolute path");
        let created_path = directory.path().join("new.txt");
        let existing_path = directory.path().join("existing.txt");
        let created = file_diff_bytes(&created_path, None, b"")
            .expect("creating an empty file is still a filesystem change");
        assert_eq!(created.old_text(), None);
        assert_eq!(created.new_text(), "");

        assert_eq!(
            file_diff_bytes(&existing_path, Some(b""), b""),
            None,
            "an unchanged existing empty file is not a diff"
        );
    }

    #[test]
    fn a_bom_on_one_side_only_does_not_rewrite_the_first_line() {
        let patch = unified_diff_bytes(
            "demo.txt",
            "\u{feff}one\ntwo\n".as_bytes(),
            "one\nTWO\n".as_bytes(),
        )
        .expect("the second line differs");
        assert!(
            !patch.contains("-one"),
            "the BOM alone is not a change:\n{patch}"
        );
        assert!(patch.contains("+TWO"), "{patch}");
    }

    #[test]
    fn the_patch_the_transcript_would_colour_is_the_patch_this_produces() {
        // The viewer and the transcript both key off a `@@` hunk header plus a `+`/`-`
        // line; asserting it here keeps this module from emitting something the
        // renderers would not recognise as a diff.
        let patch = unified_diff("demo.rs", "a\n", "b\n").expect("a change");
        assert!(patch.lines().any(|line| line.starts_with("@@")));
        assert!(
            patch
                .lines()
                .any(|line| line.starts_with('+') || line.starts_with('-'))
        );
    }
}

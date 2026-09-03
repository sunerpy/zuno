use super::*;
use std::fs;
use zuno_llm::event::RequestContentBlock;
use zuno_tui::views::autocomplete::CandidateKind;

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temp project");
    fs::create_dir(root.path().join(".git")).expect("git marker");
    fs::create_dir(root.path().join("src")).expect("source directory");
    fs::write(root.path().join("src/main.rs"), "fn real_file() {}\n").expect("source file");
    fs::write(root.path().join("src/ignored.rs"), "fn ignored() {}\n")
        .expect("ignored source file");
    fs::write(root.path().join(".gitignore"), "src/ignored.rs\n").expect("ignore rules");
    root
}

fn displays(source: &ProjectFiles) -> Vec<String> {
    source
        .candidates(Trigger::Reference, "")
        .into_iter()
        .map(|candidate| candidate.display)
        .collect()
}

#[test]
fn project_files_offer_real_files_and_directories_but_not_gitignored_entries() {
    let root = fixture();
    let source = ProjectFiles::build(root.path(), None).expect("index project files");
    let offered = displays(&source);

    assert!(offered.contains(&"src/".to_owned()), "{offered:?}");
    assert!(offered.contains(&"src/main.rs".to_owned()), "{offered:?}");
    assert!(
        !offered.contains(&"src/ignored.rs".to_owned()),
        "a gitignored file was offered: {offered:?}"
    );
    assert!(
        source
            .candidates(Trigger::Reference, "")
            .iter()
            .any(|candidate| candidate.kind == CandidateKind::Directory),
        "the real directory was not typed as a directory"
    );
}

/// `watcher.ignore` is documented configuration, and the only way to tell that the
/// TUI read it is that a pattern set nowhere else changes what the index offers.
/// `src/main.rs` is not gitignored and is offered when the block is absent, so its
/// disappearance can only come from the configured pattern reaching the filter.
#[test]
fn project_files_honour_the_configured_watcher_ignore_patterns() {
    let root = fixture();
    let watcher = zuno_config::schema::WatcherConfig {
        ignore: Some(vec!["src/main.rs".to_owned()]),
    };

    let unconfigured = displays(&ProjectFiles::build(root.path(), None).expect("default index"));
    let configured =
        displays(&ProjectFiles::build(root.path(), Some(&watcher)).expect("configured index"));

    assert!(
        unconfigured.contains(&"src/main.rs".to_owned()),
        "the fixture file must be offered without a watcher block: {unconfigured:?}"
    );
    assert!(
        !configured.contains(&"src/main.rs".to_owned()),
        "`watcher.ignore` did not reach the reference filter: {configured:?}"
    );
    assert!(
        configured.contains(&"src/".to_owned()),
        "the pattern excluded more than it named: {configured:?}"
    );
}

#[test]
fn project_file_index_never_exceeds_its_candidate_cap() {
    let root = tempfile::tempdir().expect("temp project");
    for index in 0..8 {
        fs::write(
            root.path().join(format!("file-{index}.txt")),
            index.to_string(),
        )
        .expect("fixture file");
    }

    let source = ProjectFiles::build_with_limits(root.path(), None, 20, 3).expect("bounded index");
    assert_eq!(
        source.candidates(Trigger::Reference, "").len(),
        3,
        "the index retained more candidates than its hard cap"
    );
}

#[test]
fn project_file_index_stops_at_the_scan_cap_before_the_candidate_cap() {
    let root = tempfile::tempdir().expect("temp project");
    for index in 0..8 {
        fs::write(
            root.path().join(format!("file-{index}.txt")),
            index.to_string(),
        )
        .expect("fixture file");
    }

    assert_eq!(REFERENCE_SCAN_LIMIT, 20_000);
    let source = ProjectFiles::build_with_limits(root.path(), None, 3, 20).expect("bounded scan");
    assert_eq!(
        source.candidates(Trigger::Reference, "").len(),
        2,
        "the root plus two files must exhaust the three-entry scan budget"
    );
}

#[tokio::test]
async fn text_reference_becomes_provider_text_with_the_real_file_body() {
    let root = fixture();
    let resolved = resolve_submission(
        root.path(),
        PromptSubmission::Text("explain @src/main.rs".to_owned()),
    )
    .await
    .expect("resolve text reference");

    let PromptSubmission::Content { text, content } = resolved else {
        panic!("a referenced file was still a literal text submission");
    };
    assert_eq!(text, "explain @src/main.rs");
    assert!(content.iter().any(|block| matches!(
        block,
        RequestContentBlock::Text { text }
            if text.contains("src/main.rs") && text.contains("fn real_file() {}")
    )));
}

#[tokio::test]
async fn missing_reference_is_visible_in_the_error_and_produces_no_submission() {
    let root = fixture();
    let result = resolve_submission(
        root.path(),
        PromptSubmission::Text("read @src/missing.rs".to_owned()),
    )
    .await;

    let error = result.expect_err("a missing reference must refuse the turn");
    assert!(error.contains("src/missing.rs"), "{error}");
    assert!(error.contains("not found"), "{error}");
}

#[tokio::test]
async fn single_line_reference_over_the_byte_limit_is_refused_with_that_limit_named() {
    let root = fixture();
    let body = vec![b'x'; REFERENCE_MAX_BYTES + 1];
    assert_eq!(body.iter().filter(|byte| **byte == b'\n').count(), 0);
    fs::write(root.path().join("large.txt"), body).expect("large fixture");

    let error = resolve_submission(
        root.path(),
        PromptSubmission::Text("inspect @large.txt".to_owned()),
    )
    .await
    .expect_err("an oversized reference must be refused rather than truncated");
    assert!(error.contains("51,200-byte"), "{error}");
}

#[tokio::test]
async fn many_short_lines_under_the_byte_limit_are_refused_with_the_line_limit_named() {
    let root = fixture();
    let body = "x\n".repeat(REFERENCE_MAX_LINES + 1);
    assert!(body.len() < REFERENCE_MAX_BYTES);
    fs::write(root.path().join("many-lines.txt"), body).expect("many-line fixture");

    let error = resolve_submission(
        root.path(),
        PromptSubmission::Text("inspect @many-lines.txt".to_owned()),
    )
    .await
    .expect_err("a reference over the line limit must be refused");
    assert!(error.contains("2001 lines"), "{error}");
    assert!(error.contains("2000-line limit"), "{error}");
}

#[tokio::test]
async fn reference_just_under_both_byte_and_line_limits_is_accepted() {
    let root = fixture();
    let mut body = "x\n".repeat(REFERENCE_MAX_LINES - 2);
    body.push_str(&"y".repeat(REFERENCE_MAX_BYTES - 1 - body.len()));
    assert_eq!(body.len(), REFERENCE_MAX_BYTES - 1);
    assert_eq!(body.lines().count(), REFERENCE_MAX_LINES - 1);
    fs::write(root.path().join("within-limits.txt"), &body).expect("boundary fixture");

    let resolved = resolve_submission(
        root.path(),
        PromptSubmission::Text("inspect @within-limits.txt".to_owned()),
    )
    .await
    .expect("a reference below both limits must be accepted");
    let PromptSubmission::Content { content, .. } = resolved else {
        panic!("an accepted boundary reference did not become rich content");
    };
    assert!(content.iter().any(|block| matches!(
        block,
        RequestContentBlock::Text { text }
            if text.contains("BEGIN REFERENCED FILE: within-limits.txt")
                && text.contains(&body)
    )));
}

#[tokio::test]
async fn prompt_over_the_reference_count_limit_is_refused_with_that_limit_named() {
    let root = fixture();
    let references = (0..=REFERENCE_MAX_PER_PROMPT)
        .map(|index| {
            let name = format!("reference-{index}.txt");
            fs::write(root.path().join(&name), index.to_string()).expect("reference fixture");
            format!("@{name}")
        })
        .collect::<Vec<_>>();

    let error = resolve_submission(
        root.path(),
        PromptSubmission::Text(format!("inspect {}", references.join(" "))),
    )
    .await
    .expect_err("a prompt over the reference count limit must be refused");
    assert!(error.contains("at most 16 files"), "{error}");
}

#[tokio::test]
async fn image_reference_becomes_an_image_content_block() {
    let root = fixture();
    let bytes = b"\x89PNG\r\n\x1a\nfixture";
    fs::write(root.path().join("diagram.png"), bytes).expect("image fixture");

    let resolved = resolve_submission(
        root.path(),
        PromptSubmission::Text("describe @diagram.png".to_owned()),
    )
    .await
    .expect("resolve image reference");
    let PromptSubmission::Content { content, .. } = resolved else {
        panic!("an image reference was not promoted to rich content");
    };
    assert!(content.iter().any(|block| matches!(
        block,
        RequestContentBlock::Image { media_type, data, .. }
            if media_type == "image/png" && !data.is_empty()
    )));
}

#[tokio::test]
async fn image_reference_uses_the_image_limit_instead_of_the_text_file_limit() {
    let root = fixture();
    let mut bytes = b"\x89PNG\r\n\x1a\nfixture".to_vec();
    bytes.resize(REFERENCE_MAX_BYTES + 1, 0);
    fs::write(root.path().join("large-diagram.png"), bytes).expect("large image fixture");

    let resolved = resolve_submission(
        root.path(),
        PromptSubmission::Text("describe @large-diagram.png".to_owned()),
    )
    .await
    .expect("a valid image below the image limit is accepted");

    let PromptSubmission::Content { content, .. } = resolved else {
        panic!("the image reference was not promoted to rich content");
    };
    assert!(content.iter().any(|block| matches!(
        block,
        RequestContentBlock::Image { filename, .. }
            if filename.as_deref() == Some("large-diagram.png")
    )));
}

#[tokio::test]
async fn an_existing_image_attachment_can_be_combined_with_a_project_reference() {
    let root = fixture();
    let original_image = RequestContentBlock::Image {
        filename: Some("clipboard.png".to_owned()),
        media_type: "image/png".to_owned(),
        data: "iVBORw0KGgo=".to_owned(),
    };
    let resolved = resolve_submission(
        root.path(),
        PromptSubmission::Content {
            text: "compare [Image #1] with @src/main.rs".to_owned(),
            content: vec![
                RequestContentBlock::Text {
                    text: "compare [Image #1] with @src/main.rs".to_owned(),
                },
                original_image.clone(),
            ],
        },
    )
    .await
    .expect("resolve a project reference beside an existing image");

    let PromptSubmission::Content { content, .. } = resolved else {
        panic!("rich content was downgraded while resolving a file reference");
    };
    assert!(content.contains(&original_image));
    assert!(content.iter().any(|block| matches!(
        block,
        RequestContentBlock::Text { text }
            if text.contains("src/main.rs") && text.contains("fn real_file() {}")
    )));
}

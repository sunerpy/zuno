//! Structural guards: the rules this crate must not be able to break quietly.
//!
//! Each assertion here replaces a rule that would otherwise live in a comment. A
//! comment is a suggestion; a failing test is a decision the next author has to
//! make deliberately.

use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file in this crate, with its path.
///
/// This file is excluded from its own scan. Its needles are, necessarily, the
/// literals it forbids; including it would make each guard report itself and say
/// nothing about the crate.
fn sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    for directory in ["src", "tests"] {
        collect(&crate_root().join(directory), &mut files);
    }
    let this_file = Path::new(file!())
        .file_name()
        .expect("this file has a name")
        .to_owned();
    files.retain(|(path, _)| path.file_name() != Some(this_file.as_os_str()));
    assert!(!files.is_empty(), "no sources found; the walker is wrong");
    files
}

fn collect(directory: &Path, into: &mut Vec<(PathBuf, String)>) {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
    for entry in entries {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            collect(&path, into);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            into.push((path, text));
        }
    }
}

/// Lines of `text` that contain `needle` and are not documentation or a comment.
///
/// The rules below are about *code*, and every one of them is discussed in this
/// crate's own docs. A grep that could not tell the two apart would force the
/// explanation to be deleted in order to keep the guard green.
fn offending_lines(text: &str, needle: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .filter(|line| line.contains(needle))
        .map(str::to_owned)
        .collect()
}

/// A network chunk is never decoded here.
///
/// `zuno_llm::sse::Utf8StreamDecoder` holds the boundary state that keeps a code
/// point split across two chunks intact. A `from_utf8_lossy` in this crate would
/// silently replace a truncated-but-valid sequence with U+FFFD, corrupting CJK and
/// emoji output in a way no unit test on whole bodies would catch.
#[test]
fn no_source_file_decodes_bytes_lossily() {
    for (path, text) in sources() {
        let offenders = offending_lines(&text, "from_utf8_lossy");
        assert!(
            offenders.is_empty(),
            "{} calls from_utf8_lossy: {offenders:?}\n\
             Decoding belongs to zuno_llm::sse::Utf8StreamDecoder, which is the only \
             thing that knows whether a truncated sequence is invalid or merely \
             incomplete.",
            path.display()
        );
    }
}

/// SSE framing is not reimplemented here.
///
/// Searching for a blank-line separator is the signature of a second parser. One
/// parser means one place where the CRLF-versus-LF and split-separator cases are
/// handled, and `zuno-llm` already proves those with a byte-offset sweep.
#[test]
fn no_source_file_frames_sse_itself() {
    for (path, text) in sources() {
        for needle in [r#""\n\n""#, r#""\r\n\r\n""#] {
            let offenders = offending_lines(&text, needle);
            assert!(
                offenders.is_empty(),
                "{} searches for an SSE frame separator ({needle}): {offenders:?}\n\
                 Framing belongs to zuno_llm::sse::SseParser.",
                path.display()
            );
        }
    }
}

/// Retryability is never decided by reading a rendered message.
///
/// Both reference implementations shipped `is_retryable(&message.to_lowercase())`.
/// `zuno-error` classifies from a status code and structured body fields, and this
/// asserts nobody reintroduces the string path.
#[test]
fn no_source_file_lowercases_a_message_to_classify_it() {
    for (path, text) in sources() {
        for needle in ["to_lowercase()", "to_ascii_lowercase()"] {
            for line in offending_lines(&text, needle) {
                // Normalizing a *model id* for a documented model rule is a
                // different operation from classifying an error, and the only
                // place it is permitted is the quirk table's canonicalization.
                let is_model_id_normalization = path.ends_with("quirks.rs");
                assert!(
                    is_model_id_normalization,
                    "{} lowercases text outside the model-id canonicalizer: {line}\n\
                     Error classification must read a status code or a structured \
                     field, never rendered prose.",
                    path.display()
                );
            }
        }
    }
}

/// No placeholder ships.
#[test]
fn no_source_file_contains_a_placeholder() {
    for (path, text) in sources() {
        for needle in ["todo!(", "unimplemented!("] {
            let offenders = offending_lines(&text, needle);
            assert!(
                offenders.is_empty(),
                "{} contains a placeholder: {offenders:?}",
                path.display()
            );
        }
    }
}

/// The manifest declares no error-erasing dependency and no second HTTP client.
///
/// `anyhow` would let a typed [`zuno_error::ProviderError`] be flattened into a
/// string, which is exactly what makes a recovery decision unrecoverable.
#[test]
fn the_manifest_declares_no_anyhow_and_only_one_http_client() {
    let path = crate_root().join("Cargo.toml");
    let text = std::fs::read_to_string(&path).expect("read the manifest");
    let manifest: toml::Value = toml::from_str(&text).expect("parse the manifest");

    let mut declared = Vec::new();
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(toml::Value::Table(entries)) = manifest.get(table) {
            declared.extend(entries.keys().cloned());
        }
    }

    assert!(
        !declared.iter().any(|name| name == "anyhow"),
        "anyhow erases the typed error taxonomy this workspace relies on"
    );

    let clients: Vec<&String> = declared
        .iter()
        .filter(|name| {
            ["ureq", "curl", "isahc", "surf", "attohttpc", "hyper"].contains(&name.as_str())
        })
        .collect();
    assert!(
        clients.is_empty(),
        "a second HTTP client would bypass the Transport seam: {clients:?}"
    );
}

/// The model-id table is a single named place.
///
/// `zuno-llm`'s `policy_sources_contain_no_model_id_literals` forbids model-id
/// literals in policy code. This crate genuinely has two model-id rules — Copilot's
/// documented `gpt-N` check and the reasoning-content protocol table — so the
/// discipline here is that they live in exactly two named functions and nowhere
/// else, rather than that they do not exist.
#[test]
fn model_id_literals_appear_only_in_the_two_named_rule_tables() {
    // Literals that are model ids, as opposed to provider ids or JSON keys.
    let literals = ["gpt-5-mini", "deepseek-v4"];
    let permitted: &[&str] = &["surface.rs", "quirks.rs"];

    for (path, text) in sources() {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a UTF-8 file name");
        if path.components().any(|part| part.as_os_str() == "tests") {
            // A test naming a model id is the point of the test.
            continue;
        }
        for literal in literals {
            let offenders = offending_lines(&text, literal);
            assert!(
                offenders.is_empty() || permitted.contains(&file),
                "{} names the model id `{literal}` outside the two rule tables \
                 ({permitted:?}): {offenders:?}",
                path.display()
            );
        }
    }
}

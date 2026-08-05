use std::path::{Path, PathBuf};

const BANNED_TOKENS: &[&str] = &["println!", "print!", "io::stdout", "stdout()", "Stdout"];

#[derive(Debug)]
struct Violation {
    file: PathBuf,
    line_number: usize,
    token: &'static str,
}

fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(at) if !line[..at].contains('"') => &line[..at],
        _ => line,
    }
}

fn banned_token(code: &str) -> Option<&'static str> {
    BANNED_TOKENS.iter().copied().find(|token| {
        code.match_indices(token).any(|(at, _)| {
            at == 0
                || !code[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|character| character.is_alphanumeric() || character == '_')
        })
    })
}

fn rust_sources(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("read source directory") {
        let entry = entry.expect("read source entry");
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn engine_library_has_no_print_or_stdout_path() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&source, &mut files);
    assert!(files.len() >= 2, "source scan must not pass vacuously");

    let mut violations = Vec::new();
    for file in files {
        let contents = std::fs::read_to_string(&file).expect("read Rust source");
        for (index, line) in contents.lines().enumerate() {
            if let Some(token) = banned_token(strip_line_comment(line)) {
                violations.push(Violation {
                    file: file.clone(),
                    line_number: index + 1,
                    token,
                });
            }
        }
    }

    assert!(
        violations.is_empty(),
        "oc-engine must remain renderer-free and stdout-free: {violations:#?}"
    );
}

#[test]
fn stdout_scanner_detects_each_banned_path() {
    for case in [
        r#"println!("debug");"#,
        r#"print!("debug");"#,
        "let output = std::io::stdout();",
        "fn output() -> Stdout",
    ] {
        assert!(banned_token(case).is_some(), "scanner missed {case:?}");
    }
}

#[test]
fn stdout_violation_fields_remain_reportable() {
    let violation = Violation {
        file: PathBuf::from("src/loop.rs"),
        line_number: 1,
        token: "stdout()",
    };
    assert_eq!(violation.file, PathBuf::from("src/loop.rs"));
    assert_eq!(violation.line_number, 1);
    assert_eq!(violation.token, "stdout()");
}

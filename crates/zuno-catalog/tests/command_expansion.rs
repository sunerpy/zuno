//! A differential of argument expansion against the oracle's own JavaScript.
//!
//! # Why this exists in this shape
//!
//! `expand`, `hints`, and `tokenize` are transcriptions of
//! `packages/opencode/src/session/prompt.ts:1372-1395` and
//! `packages/opencode/src/command/index.ts:36-43`. Reading that code is not the
//! same as knowing what it does — the greedy highest placeholder, `$0` rendering
//! the literal `undefined`, `$5.00` losing its digits, and `$$` collapsing
//! inside `$ARGUMENTS` are all things a careful reader misses and a run does not.
//!
//! So the expected values here were *produced*, not written:
//! `fixtures/command_expansion_oracle.cjs` is a verbatim transcription of the
//! oracle's expansion body and regexes, run over
//! `fixtures/command_expansion_cases.json` to yield
//! `fixtures/command_expansion_expected.json`.
//!
//! The golden file pins the behaviour on every machine. When Node is available,
//! [`command_expansion_golden_still_matches_the_javascript`] re-derives it and fails if
//! the golden has drifted, so the golden cannot rot into a self-consistent
//! fiction — the failure mode `zuno-testkit`'s module docs describe.
//!
//! Dispatch-time concerns the oracle handles elsewhere are deliberately out of
//! scope: the `` !`cmd` `` shell substitution
//! (`session/prompt.ts:1397-1408`) spawns processes and belongs to whichever
//! todo owns that, and it runs strictly *after* everything tested here.

use std::path::{Path, PathBuf};
use std::process::Command;
use zuno_catalog::command::{expand, hints, tokenize};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read_json(path: &Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} should be readable: {error}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("{} should be a JSON array: {error}", path.display()))
}

fn field<'a>(case: &'a serde_json::Value, key: &str) -> &'a str {
    case.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("every case needs a string {key}, got {case}"))
}

fn strings(case: &serde_json::Value, key: &str) -> Vec<String> {
    case.get(key)
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("every case needs an array {key}, got {case}"))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("{key} holds strings, got {value}"))
                .to_owned()
        })
        .collect()
}

#[test]
fn command_expansion_matches_the_oracle_on_every_case() {
    let expected = read_json(&fixtures().join("command_expansion_expected.json"));
    assert!(
        expected.len() >= 59,
        "the case table should not shrink; found {}",
        expected.len()
    );

    let mut failures = Vec::new();
    for case in &expected {
        let id = field(case, "id");
        let template = field(case, "template");
        let arguments = field(case, "arguments");

        let got = expand(template, arguments);
        let want = field(case, "expanded");
        if got != want {
            failures.push(format!(
                "[{id}] expand\n  template  {template:?}\n  arguments {arguments:?}\n  oracle    {want:?}\n  ours      {got:?}"
            ));
        }

        let got = hints(template);
        let want = strings(case, "hints");
        if got != want {
            failures.push(format!(
                "[{id}] hints\n  template {template:?}\n  oracle   {want:?}\n  ours     {got:?}"
            ));
        }

        let got = tokenize(arguments);
        let want = strings(case, "tokens");
        if got != want {
            failures.push(format!(
                "[{id}] tokenize\n  arguments {arguments:?}\n  oracle    {want:?}\n  ours      {got:?}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} cases diverged from the oracle:\n\n{}",
        failures.len(),
        expected.len(),
        failures.join("\n\n")
    );
}

/// Re-derive the golden file from the oracle's JavaScript and fail if it moved.
///
/// Skipped, loudly, when Node is not installed: a machine without Node still
/// gets the golden-file comparison above, which is the part that protects the
/// implementation.
#[test]
fn command_expansion_golden_still_matches_the_javascript() {
    let dir = fixtures();
    let script = dir.join("command_expansion_oracle.cjs");
    let cases = dir.join("command_expansion_cases.json");

    let output = match Command::new("node").arg(&script).arg(&cases).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping: node is not installed, so the golden cannot be re-derived");
            return;
        }
        Err(error) => panic!("running node failed: {error}"),
    };
    assert!(
        output.status.success(),
        "the oracle transcription failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let derived: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("the oracle transcription should emit a JSON array");
    let golden: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("command_expansion_expected.json"))
            .expect("the golden should be readable"),
    )
    .expect("the golden should be a JSON array");

    assert_eq!(
        derived, golden,
        "the golden file no longer matches what the oracle's JavaScript produces; \
         regenerate it with `node tests/fixtures/command_expansion_oracle.cjs \
         tests/fixtures/command_expansion_cases.json > \
         tests/fixtures/command_expansion_expected.json` and review the diff"
    );
}

/// A template can be adversarial without becoming a panic.
///
/// The oracle cannot panic here — JavaScript has no equivalent — so the only way
/// to preserve that property is to assert it. Every input below is a string a
/// user could plausibly type into a command template.
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

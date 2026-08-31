//! Substitution feeding the parser, which is the order the oracle uses.
//!
//! `packages/opencode/src/config/config.ts:213-227` substitutes over the file's
//! text and hands the result to `ConfigParse.jsonc`, then to the schema. These
//! tests exercise that seam: the output of [`Substitution::apply`] has to be a
//! document [`Config::from_json_str`] accepts, whatever was inside the
//! referenced files.
//!
//! Test names are prefixed `variable_` so `cargo test -p zuno-config variable`
//! selects them alongside the unit tests in `src/variable.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use zuno_config::Config;
use zuno_config::variable::{Missing, Substitution};
use zuno_error::ConfigError;
use zuno_paths::env::{Env, HOME};

struct Site {
    root: TempDir,
}

impl Site {
    fn new() -> Self {
        let root = TempDir::new().expect("temp dir");
        fs::create_dir_all(root.path().join("home")).expect("home dir");
        Self { root }
    }

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, content).expect("write fixture");
        path
    }

    fn path(&self, relative: &str) -> PathBuf {
        relative
            .split('/')
            .filter(|component| !component.is_empty())
            .fold(self.root.path().to_path_buf(), |path, component| {
                path.join(component)
            })
    }

    fn home_env(&self) -> Env {
        Env::empty().with(HOME, self.path("home").to_string_lossy())
    }
}

fn detail(error: &ConfigError) -> String {
    let ConfigError::Invalid { issues, .. } = error else {
        panic!("expected Invalid, got {error:?}");
    };
    issues
        .iter()
        .map(|issue| issue.detail.clone())
        .collect::<Vec<_>>()
        .join("; ")
}

/// The happy path from the plan: a config whose model comes from the environment
/// and whose instructions come from a `~/` file.
#[test]
fn variable_expands_env_and_tilde_file_into_a_parsable_config() {
    let site = Site::new();
    site.write("home/notes.md", "  Always cite the file you edited.  \n");
    let config = site.write(
        "zuno.json",
        r#"{
  "model": "{env:ZUNO_SAMPLE_MODEL}",
  "instructions": ["{file:~/notes.md}"]
}"#,
    );

    let env = Env::empty().with("ZUNO_SAMPLE_MODEL", "anthropic/claude-sonnet-4-5");
    let process = site.home_env();
    let expanded = Substitution::for_file(&config)
        .with_env(&env)
        .with_process_env(&process)
        .apply(&fs::read_to_string(&config).expect("read config"))
        .expect("both references resolve");

    assert_eq!(
        expanded,
        r#"{
  "model": "anthropic/claude-sonnet-4-5",
  "instructions": ["Always cite the file you edited."]
}"#
    );

    let parsed = Config::from_json_str(&config, &expanded).expect("valid config");
    assert_eq!(parsed.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
    assert_eq!(
        parsed.instructions.as_deref(),
        Some(["Always cite the file you edited.".to_owned()].as_slice())
    );
}

/// The failure path from the plan: an absent `{file:}` target must name itself,
/// not quietly vanish.
#[test]
fn variable_absent_file_reference_names_the_path_instead_of_substituting_empty() {
    let site = Site::new();
    let reference = "/nonexistent";
    let token = format!("{{file:{reference}}}");
    let document = format!(r#"{{"instructions":["{token}"]}}"#);
    let config = site.write("zuno.json", &document);

    let process = Env::empty();
    let error = Substitution::for_file(&config)
        .with_process_env(&process)
        .apply(&fs::read_to_string(&config).expect("read config"))
        .expect_err("an absent reference must fail the load");

    assert_eq!(
        detail(&error),
        format!(
            "bad file reference: \"{token}\" {} does not exist",
            PathBuf::from(zuno_paths::node_path::resolve(
                &zuno_paths::node_path::dirname(&config.to_string_lossy()),
                &[reference],
            ))
            .display()
        )
    );
    let ConfigError::Invalid { path, .. } = &error else {
        panic!("expected Invalid");
    };
    assert_eq!(path, &config);

    // And the alternative really is silence, so the distinction is worth having:
    // `tui.json` asks for exactly that.
    assert_eq!(
        Substitution::for_file(&config)
            .with_process_env(&process)
            .on_missing(Missing::Empty)
            .apply(&document)
            .expect("swallowed"),
        r#"{"instructions":[""]}"#
    );
}

/// A file body full of characters that would break a JSON string still leaves a
/// document the parser accepts, and the value survives byte for byte.
#[test]
fn variable_hostile_file_content_still_parses_as_json() {
    let site = Site::new();
    let body = "He said \"hi\".\nPath: C:\\tmp\ttabbed\r\nDone.\u{1}";
    site.write("prompt.md", &format!("\n  {body}  \n"));
    let config = site.write("zuno.json", r#"{"instructions":["{file:./prompt.md}"]}"#);

    let process = Env::empty();
    let expanded = Substitution::for_file(&config)
        .with_process_env(&process)
        .apply(&fs::read_to_string(&config).expect("read config"))
        .expect("the reference resolves");

    assert_eq!(
        expanded,
        r#"{"instructions":["He said \"hi\".\nPath: C:\\tmp\ttabbed\r\nDone.\u0001"]}"#
    );
    let parsed = Config::from_json_str(&config, &expanded).expect("still valid JSON");
    assert_eq!(
        parsed.instructions.as_deref(),
        Some([body.to_owned()].as_slice())
    );
}

/// A relative reference is resolved against the config file, not the process
/// working directory. Two configs in different directories name the same
/// relative path and get different files.
#[test]
fn variable_relative_reference_follows_the_config_file_not_the_cwd() {
    let site = Site::new();
    fs::create_dir_all(site.path("a")).expect("dir a");
    fs::create_dir_all(site.path("b")).expect("dir b");
    site.write("a/shared.md", "from-a");
    site.write("b/shared.md", "from-b");
    let process = Env::empty();
    let text = r#"{"instructions":["{file:./shared.md}"]}"#;

    for (directory, expected) in [("a", "from-a"), ("b", "from-b")] {
        let config = site.path(directory).join("zuno.json");
        let expanded = Substitution::for_file(&config)
            .with_process_env(&process)
            .apply(text)
            .expect("each config sees its own neighbour");
        assert_eq!(expanded, format!(r#"{{"instructions":["{expected}"]}}"#));
    }

    // The cwd is the crate root during `cargo test`, and it holds no `shared.md`,
    // so a cwd-based resolution could not have produced either answer.
    assert!(
        !std::env::current_dir()
            .expect("cwd")
            .join("shared.md")
            .exists()
    );
}

/// A `{file:}` token on a `//` line is left in place, and a `{env:}` token on the
/// same line is not — the asymmetry the oracle actually has.
#[test]
fn variable_comment_lines_hold_back_file_tokens_only() {
    let site = Site::new();
    site.write("prompt.md", "real");
    let config = site.write("zuno.json", "{}");
    let env = Env::empty().with("ZUNO_SAMPLE_MODEL", "expanded");
    let process = Env::empty();

    let expanded = Substitution::for_file(&config)
        .with_env(&env)
        .with_process_env(&process)
        .apply(
            "{\n  // disabled: {file:./prompt.md} {env:ZUNO_SAMPLE_MODEL}\n  \"instructions\": [\"{file:./prompt.md}\"]\n}",
        )
        .expect("the live reference resolves and the commented one is never read");

    assert_eq!(
        expanded,
        "{\n  // disabled: {file:./prompt.md} expanded\n  \"instructions\": [\"real\"]\n}"
    );
}

/// A remote config body has no file of its own; it carries a label and a base
/// directory instead, and both show up where they should.
#[test]
fn variable_virtual_source_uses_its_directory_and_reports_its_label() {
    let site = Site::new();
    site.write("header.txt", "Bearer abc");
    let dir = site.path("");
    let process = Env::empty();
    let subject = Substitution::for_virtual("https://example.test/config.json", Path::new(&dir))
        .with_process_env(&process);

    assert_eq!(
        subject
            .apply(r#"{"headers":{"Authorization":"{file:./header.txt}"}}"#)
            .expect("the reference resolves"),
        r#"{"headers":{"Authorization":"Bearer abc"}}"#
    );

    let error = subject
        .apply("{file:./absent.txt}")
        .expect_err("absent reference");
    let ConfigError::Invalid { path, .. } = &error else {
        panic!("expected Invalid");
    };
    assert_eq!(path, Path::new("https://example.test/config.json"));
}

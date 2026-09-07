//! Persisting one host's answer about running Shell without OS confinement.
//!
//! # Why this exists
//!
//! A host with no confined backend asks its question before raw mode, and accepting
//! used to set `ZUNO_SANDBOX_BACKEND=native` on the process environment only. That is
//! correct for the session and forgotten at exit, so the next `zuno` on the same
//! machine asked again — on Windows and macOS, which have no confined backend at all,
//! that is every start, forever. The offer text listed three ways to decide it up
//! front, all of them "edit a file by hand".
//!
//! # Why a third writer, and what it deliberately is not
//!
//! Two config writers already exist and neither can be reused. `mcp add` owns a
//! private `update_json_config` that inserts one MCP server and refuses JSONC rather
//! than dropping comments; `provider_setup::ConfigChange` prepares a provider block
//! with a rollback for a login that may still fail. Both are shaped around their own
//! payload. This module writes exactly one key — `sandbox.backend` — into exactly one
//! file, and shares their two real invariants rather than their code: the surrounding
//! document survives byte for byte where it is not touched, and a JSONC file is
//! refused instead of being rewritten without its comments.
//!
//! It writes the **global** config, never a project one, because a project layer is
//! forbidden from selecting `native` (`zuno_config::discovery` rejects it as
//! self-granted trust). Writing one would produce a file that fails to load.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use zuno_config::schema::sandbox::SandboxBackendSelection;

/// The `sandbox.backend` value this module persists.
///
/// Spelled through the schema type rather than as a literal so a rename cannot leave
/// a stale string here that discovery would reject.
fn native() -> &'static str {
    SandboxBackendSelection::Native.as_str()
}

/// What persisting the choice did, for the one line a user reads afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Persisted {
    /// The key was written to this file.
    Written(PathBuf),
    /// The file already selected the native backend; nothing was written.
    AlreadySelected(PathBuf),
}

impl Persisted {
    /// The sentence printed after the question is answered.
    pub(crate) fn notice(&self) -> String {
        match self {
            Self::Written(path) => format!(
                "Saved `sandbox.backend: {native}` to {path}. Future runs will not ask again; \
                 remove that key to restore the question.",
                native = native(),
                path = path.display(),
            ),
            Self::AlreadySelected(path) => format!(
                "{path} already selects the native backend; nothing was changed.",
                path = path.display(),
            ),
        }
    }
}

/// Write `sandbox.backend: native` into the global configuration file.
///
/// # Errors
///
/// When the file cannot be read or written, when it does not hold a JSON object, when
/// `sandbox` is not an object, or when only a `.jsonc` file exists — this build has no
/// comment-preserving editor, and rewriting one would silently discard the author's
/// comments, so it reports what to add by hand instead.
pub(crate) fn persist_native_backend(layout: &zuno_paths::Layout) -> Result<Persisted, String> {
    let [json, jsonc] =
        zuno_paths::Layout::file_in_directory(layout.config(), zuno_paths::CONFIG_FILE_STEM);
    if !json.exists() && jsonc.exists() {
        return Err(format!(
            "cannot safely update {}: this build has no comment-preserving JSONC editor. Add \
             `\"sandbox\": {{\"backend\": \"{native}\"}}` to it by hand, or pass `zuno \
             --sandbox-backend {native}` for this run.",
            jsonc.display(),
            native = native(),
        ));
    }
    persist_into(&json)
}

/// The same write against an explicit path, which is what tests drive.
fn persist_into(path: &Path) -> Result<Persisted, String> {
    let original = match std::fs::read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    let mut document = match original.as_deref() {
        // A missing or empty file becomes a document holding only this choice, which is
        // what a fresh install has: the offer is the user's first interaction with it.
        None | Some([]) => Value::Object(Map::new()),
        Some(contents) => serde_json::from_slice::<Value>(contents)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?,
    };
    let root = document
        .as_object_mut()
        .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?;
    let sandbox = root
        .entry("sandbox".to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| format!("{}.sandbox must be an object", path.display()))?;
    if sandbox.get("backend").and_then(Value::as_str) == Some(native()) {
        return Ok(Persisted::AlreadySelected(path.to_path_buf()));
    }
    // Only this key is replaced. `mode`, `network`, `onUnavailable`, `writableRoots`
    // and `protectedPaths` are the user's and stay exactly as they were.
    sandbox.insert("backend".to_owned(), json!(native()));

    // Parsed back through the real schema before anything is written, so a document
    // this build would refuse to load never reaches disk.
    serde_json::from_value::<zuno_config::Config>(document.clone()).map_err(|error| {
        format!(
            "writing sandbox.backend would make {} invalid: {error}",
            path.display()
        )
    })?;
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("failed to render {}: {error}", path.display()))?;
    bytes.push(b'\n');
    zuno_atomic_file::replace(path, &bytes)
        .map_err(|error| format!("failed to update {}: {error}", path.display()))?;
    Ok(Persisted::Written(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(directory: &Path) -> String {
        std::fs::read_to_string(directory.join("zuno.json")).expect("read the written config")
    }

    #[test]
    fn a_missing_config_is_created_holding_only_this_choice() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("zuno.json");

        let outcome = persist_into(&path).expect("write");

        assert_eq!(outcome, Persisted::Written(path.clone()));
        let document = serde_json::from_str::<Value>(&written(root.path())).expect("valid JSON");
        assert_eq!(document["sandbox"]["backend"], json!("native"));
        assert_eq!(
            document.as_object().expect("object").len(),
            1,
            "nothing but the choice was invented: {document}"
        );
        assert!(outcome.notice().contains("Future runs will not ask again"));
    }

    #[test]
    fn every_unrelated_setting_and_sibling_sandbox_field_survives() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("zuno.json");
        std::fs::write(
            &path,
            br#"{"formatter":false,"sandbox":{"mode":"read-only","network":"allow"},"model":"a/b"}"#,
        )
        .expect("seed config");

        persist_into(&path).expect("write");

        let document = serde_json::from_str::<Value>(&written(root.path())).expect("valid JSON");
        assert_eq!(document["formatter"], json!(false));
        assert_eq!(document["model"], json!("a/b"));
        assert_eq!(
            document["sandbox"]["mode"],
            json!("read-only"),
            "the user's mode must not be widened by choosing a backend"
        );
        assert_eq!(document["sandbox"]["network"], json!("allow"));
        assert_eq!(document["sandbox"]["backend"], json!("native"));
    }

    #[test]
    fn an_existing_native_selection_is_reported_rather_than_rewritten() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("zuno.json");
        let before = r#"{"sandbox":{"backend":"native"}}"#;
        std::fs::write(&path, before).expect("seed config");

        let outcome = persist_into(&path).expect("no write needed");

        assert_eq!(outcome, Persisted::AlreadySelected(path.clone()));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            before,
            "an idempotent answer must not reformat the file"
        );
        assert!(outcome.notice().contains("nothing was changed"));
    }

    #[test]
    fn an_auto_selection_is_replaced_because_the_user_just_chose_otherwise() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("zuno.json");
        std::fs::write(&path, br#"{"sandbox":{"backend":"auto"}}"#).expect("seed config");

        assert_eq!(
            persist_into(&path).expect("write"),
            Persisted::Written(path.clone())
        );
        let document = serde_json::from_str::<Value>(&written(root.path())).expect("valid JSON");
        assert_eq!(document["sandbox"]["backend"], json!("native"));
    }

    #[test]
    fn a_damaged_or_wrongly_shaped_document_is_refused_without_being_overwritten() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("zuno.json");

        std::fs::write(&path, b"{ not json").expect("seed config");
        let error = persist_into(&path).expect_err("a damaged file is not silently replaced");
        assert!(error.contains("failed to parse"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{ not json",
            "the original bytes must survive a refusal"
        );

        std::fs::write(&path, b"[]").expect("seed array");
        let error = persist_into(&path).expect_err("an array is not a config document");
        assert!(error.contains("must contain a JSON object"), "{error}");

        std::fs::write(&path, br#"{"sandbox":"native"}"#).expect("seed scalar sandbox");
        let error = persist_into(&path).expect_err("a scalar sandbox is not editable");
        assert!(error.contains("sandbox must be an object"), "{error}");
    }

    #[test]
    fn a_jsonc_only_install_is_told_what_to_do_by_hand() {
        let root = tempfile::tempdir().expect("temp dir");
        let env = zuno_paths::Env::empty()
            .with("HOME", root.path().to_string_lossy())
            .with("XDG_CONFIG_HOME", root.path().to_string_lossy());
        let layout = zuno_paths::Layout::resolve_with(&env, None);
        std::fs::create_dir_all(layout.config()).expect("config directory");
        let jsonc = layout.config().join("zuno.jsonc");
        std::fs::write(&jsonc, b"{ /* keep me */ }").expect("seed JSONC");

        let error = persist_native_backend(&layout).expect_err("JSONC is refused");

        assert!(error.contains("comment-preserving"), "{error}");
        assert!(error.contains("--sandbox-backend native"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&jsonc).expect("read"),
            "{ /* keep me */ }",
            "the comments must still be there"
        );
        assert!(
            !layout.config().join("zuno.json").exists(),
            "a second config that would take precedence must not appear"
        );
    }
}

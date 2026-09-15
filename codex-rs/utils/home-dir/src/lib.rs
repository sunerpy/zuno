use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
use std::path::Path;
use std::path::PathBuf;

const ZUNO_HOME_ENV: &str = "ZUNO_HOME";
const CODEX_HOME_ENV: &str = "CODEX_HOME";
const DEFAULT_ZUNO_HOME_DIR: &str = ".zuno";

/// Returns Zuno's configuration and durable-state directory.
///
/// `ZUNO_HOME` is authoritative. `CODEX_HOME` remains a lower-precedence
/// migration alias so an explicitly isolated Codex-derived setup can be tested
/// without copying credentials. When neither is set, Zuno uses `~/.zuno` and
/// does not inspect or modify `~/.codex`.
///
/// An explicitly configured path must already exist and be a directory. It is
/// canonicalized before use; the default path is not required to exist yet.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let zuno_home = non_empty_env(ZUNO_HOME_ENV);
    let codex_home = non_empty_env(CODEX_HOME_ENV);
    resolve_zuno_home(
        zuno_home
            .as_deref()
            .map(|value| (ZUNO_HOME_ENV, value))
            .or_else(|| codex_home.as_deref().map(|value| (CODEX_HOME_ENV, value))),
        home_dir().as_deref(),
    )
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn resolve_zuno_home(
    configured: Option<(&str, &str)>,
    default_home: Option<&Path>,
) -> std::io::Result<AbsolutePathBuf> {
    match configured {
        Some((name, value)) => resolve_configured_home(name, value),
        None => {
            let mut path = default_home.map(Path::to_path_buf).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find home directory",
                )
            })?;
            path.push(DEFAULT_ZUNO_HOME_DIR);
            AbsolutePathBuf::from_absolute_path(path)
        }
    }
}

fn resolve_configured_home(name: &str, value: &str) -> std::io::Result<AbsolutePathBuf> {
    let path = PathBuf::from(value);
    let metadata = std::fs::metadata(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{name} points to {value:?}, but that path does not exist"),
        ),
        _ => std::io::Error::new(
            error.kind(),
            format!("failed to read {name} {value:?}: {error}"),
        ),
    })?;

    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{name} points to {value:?}, but that path is not a directory"),
        ));
    }

    let canonical = path.canonicalize().map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("failed to canonicalize {name} {value:?}: {error}"),
        )
    })?;
    AbsolutePathBuf::from_absolute_path(canonical)
}

#[cfg(test)]
mod tests {
    use super::CODEX_HOME_ENV;
    use super::DEFAULT_ZUNO_HOME_DIR;
    use super::ZUNO_HOME_ENV;
    use super::resolve_zuno_home;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::ErrorKind;
    use tempfile::TempDir;

    #[test]
    fn configured_missing_path_is_fatal_and_names_source() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-zuno-home");
        let missing_str = missing
            .to_str()
            .expect("missing Zuno home path should be valid utf-8");

        let error = resolve_zuno_home(Some((ZUNO_HOME_ENV, missing_str)), Some(temp_home.path()))
            .expect_err("missing ZUNO_HOME");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(
            error.to_string().contains(ZUNO_HOME_ENV),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn configured_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("zuno-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file Zuno home path should be valid utf-8");

        let error = resolve_zuno_home(Some((CODEX_HOME_ENV, file_str)), Some(temp_home.path()))
            .expect_err("file CODEX_HOME alias");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("not a directory"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn configured_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp Zuno home path should be valid utf-8");

        let resolved = resolve_zuno_home(Some((ZUNO_HOME_ENV, temp_str)), Some(temp_home.path()))
            .expect("valid ZUNO_HOME");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn no_configured_path_uses_separate_zuno_home() {
        let temp_home = TempDir::new().expect("temp home");
        let resolved = resolve_zuno_home(/*configured*/ None, Some(temp_home.path()))
            .expect("default Zuno home");
        let expected =
            AbsolutePathBuf::from_absolute_path(temp_home.path().join(DEFAULT_ZUNO_HOME_DIR))
                .expect("absolute home");
        assert_eq!(resolved, expected);
    }
}

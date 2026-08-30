use std::fs;
use std::path::Path;

use serde_json::json;
use tempfile::tempdir;
use zuno_extension::{
    API_VERSION, InstallMode, STATIC_DIRECTORY, STATIC_MANIFEST, install_local, remove_installed,
};

fn write_package(root: &Path, description: &str) {
    fs::create_dir_all(root).expect("package root");
    fs::write(
        root.join(STATIC_MANIFEST),
        serde_json::to_vec_pretty(&json!({
            "apiVersion": API_VERSION,
            "id": "review-kit",
            "description": description,
            "workflows": {
                "review": {
                    "prompt": "Review this change."
                }
            }
        }))
        .expect("manifest JSON"),
    )
    .expect("write manifest");
    fs::write(root.join("guide.md"), "review guidance").expect("write package file");
}

#[test]
fn add_update_and_remove_are_complete_directory_transitions() {
    let fixture = tempdir().expect("fixture");
    let source = fixture.path().join("source");
    let config = fixture.path().join("config");
    write_package(&source, "first");

    let installed =
        install_local(&source, &config, InstallMode::Add).expect("first install succeeds");
    assert_eq!(
        installed.destination,
        config.join(STATIC_DIRECTORY).join("review-kit")
    );
    assert_eq!(
        fs::read_to_string(installed.destination.join("guide.md")).expect("copied guide"),
        "review guidance"
    );
    let duplicate =
        install_local(&source, &config, InstallMode::Add).expect_err("add is create-only");
    assert!(duplicate.to_string().contains("already installed"));

    write_package(&source, "second");
    install_local(&source, &config, InstallMode::Update).expect("update replaces atomically");
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(installed.destination.join(STATIC_MANIFEST)).expect("updated manifest"),
    )
    .expect("manifest JSON");
    assert_eq!(manifest["description"], "second");
    assert!(
        fs::read_dir(config.join(STATIC_DIRECTORY))
            .expect("extension directory")
            .all(|entry| !entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .contains("backup")),
        "successful update left a rollback directory"
    );

    let removed = remove_installed("review-kit", &config).expect("remove succeeds");
    assert_eq!(removed, installed.destination);
    assert!(!removed.exists());
}

#[cfg(unix)]
#[test]
fn installation_rejects_symbolic_links_anywhere_in_the_package() {
    use std::os::unix::fs::symlink;

    let fixture = tempdir().expect("fixture");
    let source = fixture.path().join("source");
    let config = fixture.path().join("config");
    write_package(&source, "symlink");
    fs::write(fixture.path().join("outside"), "secret").expect("outside file");
    symlink(fixture.path().join("outside"), source.join("escape")).expect("create symlink");

    let error =
        install_local(&source, &config, InstallMode::Add).expect_err("symlink must be rejected");

    assert!(error.to_string().contains("symbolic link"));
    assert!(!config.join(STATIC_DIRECTORY).join("review-kit").exists());
    assert!(
        fs::read_dir(config.join(STATIC_DIRECTORY))
            .expect("extension directory")
            .next()
            .is_none(),
        "failed installation left a staging directory"
    );
}

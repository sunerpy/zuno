use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use zuno_enterprise::config::{Definition, ServiceConfig, ServiceRole};

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn deployment_examples_decode_and_worker_rejects_control_plane_credentials() {
    let root = repository();
    let definition: Definition = serde_json::from_slice(
        &std::fs::read(root.join("enterprise/examples/definition.json")).unwrap(),
    )
    .unwrap();
    definition.validate().unwrap();
    let mut changed = definition.clone();
    changed.budget.tokens = std::num::NonZeroU64::new(999999).unwrap();
    assert_ne!(
        definition.reference(),
        changed.reference(),
        "budget changes must change the pinned definition"
    );
    for name in ["worker", "gateway"] {
        let bytes = std::fs::read(root.join(format!("enterprise/examples/{name}.json"))).unwrap();
        let parsed: ServiceConfig = serde_json::from_slice(&bytes).unwrap();
        assert!(parsed.state_directory.is_absolute());
        if name == "worker" {
            assert!(matches!(parsed.service, ServiceRole::Worker(_)));
            let mut forged: Value = serde_json::from_slice(&bytes).unwrap();
            forged["service"]["database"] = json!({"urlFile":"/run/secrets/database.url"});
            assert!(
                serde_json::from_value::<ServiceConfig>(forged).is_err(),
                "a Worker configuration must not silently accept database credentials"
            );
        }
    }
}

#[tokio::test]
async fn config_files_are_bounded_and_relative_paths_are_refused() {
    assert!(
        zuno_enterprise::config::read_file(Path::new("relative.json"), 16)
            .await
            .is_err()
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("oversized");
    std::fs::write(&path, b"12345").unwrap();
    assert!(zuno_enterprise::config::read_file(&path, 4).await.is_err());
    let secret = directory.path().join("secret");
    std::fs::write(&secret, b"one\ntwo").unwrap();
    assert!(zuno_enterprise::config::secret(&secret).await.is_err());
}

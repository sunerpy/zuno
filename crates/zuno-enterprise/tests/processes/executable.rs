//! Select and attest the exact unpacked artifact when invoked by the release
//! smoke driver. Ordinary source tests retain Cargo's test binary.
use super::*;
use std::{
    io::Read,
    sync::{Mutex, OnceLock},
};

static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
static ROLES: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn digest(path: &Path) -> String {
    let mut file = std::fs::File::open(path).unwrap();
    let mut digest = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
pub fn binary() -> &'static Path {
    EXECUTABLE.get_or_init(|| {
        let Some(path) = std::env::var_os("ZUNO_ENTERPRISE_TEST_BINARY") else {
            return PathBuf::from(env!("CARGO_BIN_EXE_zuno-enterprise"));
        };
        let path = PathBuf::from(path);
        assert!(
            path.is_absolute() && std::fs::symlink_metadata(&path).unwrap().is_file(),
            "artifact must be an explicit regular binary"
        );
        assert_eq!(
            digest(&path),
            std::env::var("ZUNO_ENTERPRISE_TEST_BINARY_SHA256").expect("artifact digest")
        );
        let output = std::process::Command::new(&path)
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!(
                "zuno-enterprise {}",
                std::env::var("ZUNO_ENTERPRISE_TEST_VERSION").expect("artifact version")
            )
        );
        path
    })
}
pub fn started(name: &str, child: &tokio::process::Child) {
    if std::env::var_os("ZUNO_ENTERPRISE_TEST_BINARY").is_none() {
        return;
    }
    let selected = std::fs::canonicalize(binary()).unwrap();
    let actual = std::fs::read_link(format!("/proc/{}/exe", child.id().unwrap())).unwrap();
    assert_eq!(
        actual, selected,
        "{name} must execute the unpacked artifact"
    );
    ROLES.lock().unwrap().push(name.to_owned());
}
pub fn finish(model_requests: usize) {
    let Some(proof) = std::env::var_os("ZUNO_ENTERPRISE_TEST_PROOF") else {
        assert!(
            std::env::var_os("ZUNO_ENTERPRISE_TEST_BINARY").is_none(),
            "artifact runs require evidence output"
        );
        return;
    };
    let sha = digest(binary());
    assert_eq!(
        sha,
        std::env::var("ZUNO_ENTERPRISE_TEST_BINARY_SHA256").unwrap(),
        "artifact changed during smoke"
    );
    let mut roles = ROLES.lock().unwrap().clone();
    roles.sort();
    assert_eq!(roles, ["control", "gateway", "worker-a", "worker-b"]);
    write(
        Path::new(&proof),
        serde_json::to_vec_pretty(&json!({
            "schemaVersion":1,"kind":"enterprise-native-processes","binarySha256":sha,
            "version":std::env::var("ZUNO_ENTERPRISE_TEST_VERSION").unwrap(),
            "roles":roles,"modelRequests":model_requests,"userIsolation":true,
            "humanApproval":true,"workflow":true,"council":true,"workspaceMerge":true,
            "contentReview":true,"shutdown":true
        }))
        .unwrap(),
    );
}

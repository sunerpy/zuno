use std::path::Path;
use std::process::Command;

#[test]
fn wasm_runtime_is_absent_from_the_default_dependency_graph() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("zuno-plugin must be two levels below the workspace root");
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace)
        .env("CARGO_NET_OFFLINE", "true")
        .args([
            "tree",
            "--locked",
            "--offline",
            "-p",
            "zuno-plugin",
            "--no-default-features",
            "-e",
            "normal",
            "--prefix",
            "none",
        ])
        .output()
        .expect("cargo tree must be runnable");
    assert!(
        output.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout).expect("cargo tree output must be UTF-8");
    assert!(tree.lines().any(|line| line.starts_with("zuno-plugin ")));
    assert!(
        !tree.lines().any(|line| line.starts_with("wasmtime ")),
        "the default zuno-plugin graph unexpectedly contains wasmtime:\n{tree}"
    );
}

use super::*;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn replacement_keeps_the_same_writer_lock() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("memory.md");
    let guard = PathWriteGuard::try_acquire(&path).expect("first writer");
    super::super::replace(guard.destination(), b"complete first version").expect("publish");
    assert_eq!(
        PathWriteGuard::try_acquire(&path)
            .expect_err("replacement must not release write authority")
            .kind(),
        io::ErrorKind::WouldBlock,
    );
    drop(guard);
    let next = PathWriteGuard::try_acquire(&path).expect("next writer");
    assert_eq!(
        fs::read(next.destination()).expect("published content"),
        b"complete first version",
    );
}

#[test]
fn relative_parent_aliases_share_the_lock() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("memory.md");
    let alias = directory.path().join(".").join("memory.md");
    let _guard = PathWriteGuard::try_acquire(&path).expect("writer");
    assert_eq!(
        PathWriteGuard::try_acquire(&alias)
            .expect_err("the alias names the same destination")
            .kind(),
        io::ErrorKind::WouldBlock,
    );
}

#[test]
fn writer_authority_is_shared_between_processes() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("memory.md");
    let guard = PathWriteGuard::try_acquire(&path).expect("parent writer");
    let invoke = |busy: bool| {
        Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "write_guard::tests::writer_lock_child",
                "--nocapture",
            ])
            .env("ZUNO_TEST_WRITE_LOCK_PATH", &path)
            .env("ZUNO_TEST_WRITE_LOCK_BUSY", if busy { "1" } else { "0" })
            .output()
            .expect("run child")
    };
    let assert_checked = |output: std::process::Output| {
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("writer-lock-child-verified"),
            "the child test must actually run: {output:?}",
        );
    };
    assert_checked(invoke(true));
    drop(guard);
    assert_checked(invoke(false));
}

#[test]
fn writer_lock_child() {
    let Some(path) = std::env::var_os("ZUNO_TEST_WRITE_LOCK_PATH") else {
        return;
    };
    let result = PathWriteGuard::try_acquire(Path::new(&path));
    if std::env::var("ZUNO_TEST_WRITE_LOCK_BUSY").as_deref() == Ok("1") {
        assert_eq!(
            result.expect_err("parent must retain the lock").kind(),
            io::ErrorKind::WouldBlock,
        );
    } else {
        assert!(result.is_ok(), "released parent lock: {result:?}");
    }
    println!("writer-lock-child-verified");
}

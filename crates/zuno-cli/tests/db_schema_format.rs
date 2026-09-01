use std::path::Path;
use std::process::{Command, Output};

fn run_db(root: &Path, database: &Path, query: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zuno"));
    command
        .args(["db", "--format", "json", query])
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("ZUNO_DB", database)
        .env("ZUNO_DISABLE_AUTOUPDATE", "true")
        .env("ZUNO_DISABLE_MODELS_FETCH", "true");
    command.output().expect("run the production db command")
}

fn create_current_database(root: &Path, database: &Path) {
    let output = run_db(root, database, "SELECT 1 AS initialized");
    assert!(
        output.status.success(),
        "the production command did not initialize its database: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn current_format_reopens_through_the_db_command() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("current.db");
    create_current_database(root.path(), &database);

    let output = run_db(root.path(), &database, "SELECT 1 AS ok");
    assert!(
        output.status.success(),
        "current format was refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"ok\": 1"));
}

#[test]
fn unmarked_pre_release_format_is_refused_before_serving_a_query() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("unmarked.db");
    let connection = zuno_db::open::open_at(&database).expect("open old database");
    connection
        .execute_batch(
            "CREATE TABLE session (id text PRIMARY KEY, title text NOT NULL);
             INSERT INTO session VALUES ('ses_old', 'keep me');
             CREATE TABLE migration (id text PRIMARY KEY);",
        )
        .expect("create retired pre-release format");

    let output = run_db(root.path(), &database, "SELECT 1 AS should_not_be_served");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "old format was accepted: {stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "query ran before format validation: {stdout}"
    );
    assert!(stderr.contains("unsupported schema format"), "{stderr}");
    assert!(stderr.contains("preserve the database"), "{stderr}");
    assert!(stderr.contains("validated forward migration"), "{stderr}");
    assert!(!stderr.contains("rebuild"), "{stderr}");

    let title: String = connection
        .query_row("SELECT title FROM session", [], |row| row.get(0))
        .expect("rejection preserves existing data");
    assert_eq!(title, "keep me");
}

#[test]
fn another_marked_format_is_refused_with_expected_and_observed_values() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("other.db");
    create_current_database(root.path(), &database);
    let connection = zuno_db::open::open_at(&database).expect("open initialized database");
    connection
        .execute("UPDATE zuno_schema SET format = 99 WHERE singleton = 1", [])
        .expect("change format marker");

    let output = run_db(root.path(), &database, "SELECT 1 AS should_not_be_served");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "wrong format was accepted: {stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "query ran despite mismatch: {stdout}"
    );
    assert!(
        stderr.contains(&format!("expected {}", zuno_db::migration::CURRENT_FORMAT)),
        "{stderr}"
    );
    assert!(stderr.contains("Some(99)"), "{stderr}");
}

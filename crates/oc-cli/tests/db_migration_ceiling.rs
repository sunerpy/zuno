use std::path::Path;
use std::process::{Command, Output};

const FUTURE_MIGRATION_ID: &str = "99999999999999_future_migration";
const BELOW_CEILING_UNKNOWN_ID: &str = "20260622202449_unknown_gap";

fn migration_ceiling() -> &'static str {
    oc_db::migration::MIGRATION_IDS
        .iter()
        .copied()
        .max()
        .expect("the production migration set is not empty")
}

fn run_db(root: &Path, database: &Path, query: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencode-rust"));
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
        .env("OPENCODE_DB", database)
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true");
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

fn add_journal_id(database: &Path, id: &str) {
    let connection = oc_db::open::open_at(database).expect("open the initialized database");
    connection
        .execute(
            "INSERT INTO migration (id, time_completed) VALUES (?1, 0)",
            [id],
        )
        .expect("add the journal id");
}

#[test]
fn future_migration_in_the_journal_is_refused_before_the_db_command_serves_a_query() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("future.db");
    create_current_database(root.path(), &database);
    add_journal_id(&database, FUTURE_MIGRATION_ID);

    let output = run_db(root.path(), &database, "SELECT 1 AS should_not_be_served");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let ceiling = migration_ceiling();

    assert!(
        !output.status.success(),
        "future journal was accepted: {stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "the query was served before the journal was refused: {stdout}"
    );
    assert!(
        stderr.contains(ceiling),
        "ceiling {ceiling} absent: {stderr}"
    );
    assert!(
        stderr.contains(FUTURE_MIGRATION_ID),
        "observed id {FUTURE_MIGRATION_ID} absent: {stderr}"
    );
}

#[test]
fn unknown_migration_below_the_ceiling_is_tolerated_by_the_db_command() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("below-ceiling.db");
    create_current_database(root.path(), &database);
    assert!(BELOW_CEILING_UNKNOWN_ID < migration_ceiling());
    assert!(!oc_db::migration::MIGRATION_IDS.contains(&BELOW_CEILING_UNKNOWN_ID));
    add_journal_id(&database, BELOW_CEILING_UNKNOWN_ID);

    let output = run_db(root.path(), &database, "SELECT 1 AS ok");
    assert!(
        output.status.success(),
        "a journal gap below the ceiling was refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"ok\": 1"));
}

#[test]
fn compatible_journal_still_opens_through_the_db_command() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("compatible.db");
    create_current_database(root.path(), &database);

    let output = run_db(root.path(), &database, "SELECT 1 AS ok");
    assert!(
        output.status.success(),
        "compatible journal was refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"ok\": 1"));
}

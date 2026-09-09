use std::sync::Arc;
use zuno_db::{Pool, migration};
use zuno_paths::DbLocation;

#[test]
fn format_ten_query_time_indexes_are_replaced_without_losing_learning_rows() {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
    {
        let mut db = pool.get().expect("connection");
        db.execute_batch(concat!(
            include_str!("fixtures/format-7.sql"),
            include_str!("fixtures/format-8.sql"),
            include_str!("fixtures/format-9.sql"),
            include_str!("fixtures/format-10.sql"),
        ))
        .expect("exact released fixture");
        db.execute_batch(
            "CREATE VIRTUAL TABLE experience_search_fts USING fts5(title,summary,resolution,
                content='experience_record',content_rowid='rowid',tokenize='unicode61');
             CREATE TRIGGER experience_search_fts_update AFTER UPDATE ON experience_record
                BEGIN SELECT 1; END;",
        )
        .expect("old derived index");
        let before: String = db
            .query_row(
                "SELECT summary FROM experience_record WHERE id='exp_fixture_0001'",
                [],
                |row| row.get(0),
            )
            .expect("before");
        migration::apply(&mut db).expect("atomic upgrade");
        let after: String = db
            .query_row(
                "SELECT summary FROM experience_record WHERE id='exp_fixture_0001'",
                [],
                |row| row.get(0),
            )
            .expect("after");
        assert_eq!(before, after);
        migration::apply(&mut db).expect("current shape validates");
    }
    let records = zuno_db::experience::ExperienceStore::new(pool)
        .search("prj_fixture_0001", "database", 5)
        .expect("rebuilt search");
    assert_eq!(records[0].projection.id, "exp_fixture_0001");
}

#[test]
fn a_correct_marker_cannot_hide_broken_incremental_triggers_or_partial_indexes() {
    for mutation in [
        "DROP TRIGGER experience_search_cjk_fts_update;
         CREATE TRIGGER experience_search_cjk_fts_update AFTER UPDATE ON experience_record
           BEGIN SELECT 1; END;",
        "DROP INDEX message_session_user_boundary_idx;
         CREATE INDEX message_session_user_boundary_idx ON message(session_id,time_created DESC,id DESC);",
    ] {
        let pool=Pool::open(&DbLocation::Memory).expect("pool");
        let mut db=pool.get().expect("connection");
        migration::apply(&mut db).expect("current schema");
        db.execute_batch(mutation).expect("corrupt derived contract");
        let before:i64=db.query_row("SELECT total_changes()",[],|row|row.get(0)).expect("changes");
        assert!(migration::apply(&mut db).is_err());
        let after:i64=db.query_row("SELECT total_changes()",[],|row|row.get(0)).expect("changes");
        assert_eq!(before,after,"validation must not repair or mutate a corrupt marked database");
    }
}

use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use zuno_db::experience::{
    ExperienceEvidenceKind, ExperienceStore, NewExperience, NewExperienceEvidence,
};
use zuno_db::{ExperienceMatch, Pool, migration};
use zuno_paths::DbLocation;
use zuno_types::ExperienceKind;

fn fixture() -> (Arc<Pool>, ExperienceStore) {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("database"));
    {
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                 VALUES ('project', '/workspace', 1, 1, '[]'),
                        ('other', '/other', 1, 1, '[]');",
            )
            .expect("projects");
    }
    let store = ExperienceStore::new(Arc::clone(&pool));
    store
        .create_manual(record(
            "english",
            "SQLite migration",
            "SQLite migration must preserve rows and update the format marker last.",
        ))
        .expect("English evidence");
    store
        .create_manual(record(
            "chinese",
            "数据库迁移",
            "数据库迁移应该使用事务并保留已有数据",
        ))
        .expect("Chinese evidence");
    (pool, store)
}

fn record(id: &str, title: &str, summary: &str) -> NewExperience {
    NewExperience {
        id: id.to_owned(),
        project_id: "project".to_owned(),
        session_id: None,
        source_message_id: None,
        extraction_job_id: None,
        extraction_ordinal: None,
        kind: ExperienceKind::Procedure,
        title: title.to_owned(),
        summary: summary.to_owned(),
        resolution: None,
        confidence: 9_000,
        fingerprint: id.to_owned(),
        evidence: vec![NewExperienceEvidence {
            source_digest: None,
            verified: false,
            promotion_eligible: false,
            id: format!("evidence-{id}"),
            kind: ExperienceEvidenceKind::User,
            source_id: None,
            excerpt: summary.to_owned(),
            digest: hex::encode(Sha256::digest(summary.as_bytes())),
        }],
        time_created: 10,
    }
}

#[test]
fn natural_language_and_cjk_queries_recall_the_stored_evidence() {
    let (_pool, store) = fixture();
    for (query, expected) in [
        ("Please help me fix sqlite migration again", "english"),
        ("请检查数据库迁移逻辑", "chinese"),
        ("已有数据", "chinese"),
        ("事务", "chinese"),
    ] {
        let found = store.search("project", query, 5).expect("search");
        assert_eq!(
            found
                .iter()
                .map(|record| record.projection.id.as_str())
                .collect::<Vec<_>>(),
            [expected],
            "{query}",
        );
    }
    assert!(
        store
            .search("project", "PostgreSQL replication", 5)
            .expect("unrelated")
            .is_empty()
    );
    assert!(
        store
            .search("other", "sqlite migration", 5)
            .expect("scope")
            .is_empty()
    );
}

#[test]
fn repeated_searches_succeed_on_a_query_only_connection() {
    let (pool, store) = fixture();
    pool.get()
        .expect("connection")
        .pragma_update(None, "query_only", true)
        .expect("disable writes");
    for _ in 0..2 {
        assert_eq!(
            store
                .search("project", "sqlite migration", 5)
                .expect("pure read")
                .len(),
            1
        );
        assert_eq!(
            store
                .search("project", "已有数据", 5)
                .expect("pure CJK read")
                .len(),
            1
        );
    }
}

#[test]
fn literal_all_term_search_remains_explicit_and_grammar_safe() {
    let (_pool, store) = fixture();
    assert_eq!(
        store
            .search_matching("project", "sqlite migration", 5, ExperienceMatch::All)
            .expect("all literal terms")
            .len(),
        1,
    );
    assert!(
        store
            .search_matching("project", "sqlite unavailable", 5, ExperienceMatch::All,)
            .expect("missing term")
            .is_empty()
    );
    for query in [
        "\"sqlite: OR (",
        " -- (( )) \"\" ",
        "title:other NOT sqlite",
    ] {
        store
            .search("project", query, 5)
            .expect("query text must never become FTS grammar");
    }
}

#[test]
fn incremental_indexes_follow_new_and_forgotten_records() {
    let (_pool, store) = fixture();
    store
        .search("project", "sqlite", 5)
        .expect("initial search");
    store
        .create_manual(record(
            "new",
            "验证并发更新",
            "并发写入需要保留每一个已确认的修改",
        ))
        .expect("new indexed record");
    assert_eq!(
        store
            .search("project", "并发写入", 5)
            .expect("new CJK match")
            .len(),
        1
    );
    store.forget("new", 11).expect("forget");
    assert!(
        store
            .search("project", "并发写入", 5)
            .expect("forgotten match")
            .is_empty()
    );
}

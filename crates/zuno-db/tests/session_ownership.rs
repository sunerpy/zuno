//! Ownership cannot be changed through metadata, duplicate creation, or pagination.

use std::sync::Arc;

use zuno_db::session::{ListQuery, SessionCreate, Store};
use zuno_db::session_ownership::{self, ScopedSessionStore};
use zuno_db::{Pool, migration};
use zuno_error::DbError;
use zuno_paths::DbLocation;
use zuno_types::identity::{PrincipalId, PrincipalKey, PrincipalScope, TenantId};

fn owner(tenant: &str, principal: &str) -> PrincipalKey {
    PrincipalKey {
        tenant_id: TenantId::new(tenant).expect("tenant"),
        principal_id: PrincipalId::new(principal).expect("principal"),
    }
}

fn pool() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
    let mut connection = pool.get().expect("connection");
    migration::apply(&mut connection).expect("schema");
    connection.execute_batch(
        "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes) VALUES('project','/workspace',1,1,'[]')",
    ).expect("project");
    drop(connection);
    pool
}

fn draft(id: &str) -> SessionCreate {
    SessionCreate::new(
        id,
        id,
        "project",
        "/workspace",
        "/workspace",
        "Private task",
        "0.10.29",
    )
}

#[test]
fn private_sessions_are_isolated_across_subjects_and_tenants_before_pagination() {
    let pool = pool();
    let alice = owner("organization-a", "alice");
    let bob = owner("organization-a", "bob");
    let other_alice = owner("organization-b", "alice");
    let a = ScopedSessionStore::new(pool.clone(), alice.clone());
    let b = ScopedSessionStore::new(pool.clone(), bob.clone());
    let c = ScopedSessionStore::new(pool.clone(), other_alice.clone());
    a.create(&draft("alice-session")).expect("Alice session");
    b.create(&draft("bob-session")).expect("Bob session");
    c.create(&draft("other-alice-session"))
        .expect("other tenant");
    for (store, expected) in [
        (&a, "alice-session"),
        (&b, "bob-session"),
        (&c, "other-alice-session"),
    ] {
        let page = store
            .list(&ListQuery {
                limit: Some(1),
                // An untrusted query cannot replace the store's fixed owner.
                owner: Some(owner("attacker", "anyone")),
                ..ListQuery::global()
            })
            .expect("private page");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, expected);
        assert_eq!(store.get(expected).expect("own session").id, expected);
    }
    assert!(matches!(
        a.get("bob-session"),
        Err(DbError::NotFound { .. })
    ));
    assert!(matches!(
        a.get("other-alice-session"),
        Err(DbError::NotFound { .. })
    ));
    assert!(matches!(
        c.get("alice-session"),
        Err(DbError::NotFound { .. })
    ));
}

#[test]
fn duplicate_create_cannot_transfer_an_existing_session() {
    let pool = pool();
    let a = ScopedSessionStore::new(pool.clone(), owner("organization", "alice"));
    let b = ScopedSessionStore::new(pool.clone(), owner("organization", "bob"));
    a.create(&draft("shared-id")).expect("first creation");
    assert!(matches!(
        b.create(&draft("shared-id")),
        Err(DbError::Conflict { .. })
    ));
    a.create(&draft("shared-id"))
        .expect("idempotent original owner");
    assert_eq!(
        session_ownership::get(&pool.get().unwrap(), "shared-id").unwrap(),
        owner("organization", "alice")
    );
}

#[test]
fn child_inherits_parent_owner_and_mismatch_rolls_back_the_whole_creation() {
    let pool = pool();
    let alice = owner("organization", "alice");
    let a = ScopedSessionStore::new(pool.clone(), alice.clone());
    a.create(&draft("parent")).expect("parent");
    let mut child = draft("child");
    child.parent_id = Some("parent".to_owned());
    // Existing same-process child hosts use ordinary Store and inherit the owner.
    Store::new(&pool).create(&child).expect("inherited child");
    assert_eq!(
        session_ownership::get(&pool.get().unwrap(), "child").unwrap(),
        alice
    );

    child.id = "rejected-child".to_owned();
    let b = ScopedSessionStore::new(pool.clone(), owner("organization", "bob"));
    assert!(matches!(b.create(&child), Err(DbError::Conflict { .. })));
    assert!(matches!(
        Store::new(&pool).get("rejected-child"),
        Err(DbError::NotFound { .. })
    ));
    assert!(matches!(
        session_ownership::get(&pool.get().unwrap(), "rejected-child"),
        Err(DbError::NotFound { .. })
    ));
}

#[test]
fn scoped_creation_requires_an_owned_existing_parent() {
    let pool = pool();
    let a = ScopedSessionStore::new(pool.clone(), owner("organization", "alice"));
    let mut child = draft("orphan");
    child.parent_id = Some("missing-parent".to_owned());
    assert!(a.create(&child).is_err());
    assert!(Store::new(&pool).get("orphan").is_err());
    assert!(
        a.create(&draft("foreign").with_owner(owner("organization", "bob")))
            .is_err()
    );
    assert!(Store::new(&pool).get("foreign").is_err());
}

#[test]
fn direct_local_creation_and_deletion_preserve_the_ownership_invariant() {
    let pool = pool();
    let connection = pool.get().unwrap();
    connection.execute_batch(
        "INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
         VALUES('local-session','project','slug','/workspace','Local','0.10.29',1,1)",
    ).expect("legacy local insert");
    assert_eq!(
        session_ownership::get(&connection, "local-session").unwrap(),
        PrincipalScope::local().owner()
    );
    connection
        .execute("DELETE FROM session WHERE id='local-session'", [])
        .expect("delete");
    assert!(matches!(
        session_ownership::get(&connection, "local-session"),
        Err(DbError::NotFound { .. })
    ));
}

#[test]
fn corrupted_ownership_is_rejected_without_falling_back_to_local() {
    let pool = pool();
    Store::new(&pool).create(&draft("session")).expect("local");
    let connection = pool.get().unwrap();
    connection
        .execute(
            "UPDATE session_ownership SET tenant_id='../other' WHERE session_id='session'",
            [],
        )
        .expect("simulate corruption");
    assert!(session_ownership::get(&connection, "session").is_err());
}

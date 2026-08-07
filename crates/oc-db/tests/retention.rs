use std::collections::{BTreeMap, BTreeSet};

use oc_db::retention::{
    DAY_MILLIS, ExclusionReason, Liveness, LivenessProbe, ProtectionReason, RetentionKey,
    RetentionRequest, RetentionScope, SelectionReason, select,
};
use oc_db::{Connection, Pool, migration};
use oc_paths::DbLocation;
use proptest::prelude::*;

const NOW: i64 = 200 * DAY_MILLIS;
const OLD: i64 = 10 * DAY_MILLIS;
const NEW: i64 = 190 * DAY_MILLIS;

#[derive(Clone)]
struct FakeProbe(Liveness);

impl FakeProbe {
    fn reachable(ids: &[&str]) -> Self {
        Self(Liveness::Reachable {
            active_session_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        })
    }

    fn unreachable() -> Self {
        Self(Liveness::Unreachable)
    }
}

impl LivenessProbe for FakeProbe {
    fn probe(&self) -> Liveness {
        self.0.clone()
    }
}

fn pool() -> Pool {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    {
        let mut connection = pool.get().expect("check out a connection");
        migration::apply(&mut connection).expect("apply schema");
    }
    pool
}

fn insert_project(connection: &Connection, id: &str) {
    connection
        .execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES (?1, ?2, 1, 1, '[]')",
            rusqlite::params![id, format!("/srv/{id}")],
        )
        .expect("insert project");
}

#[derive(Default)]
struct SessionSeed<'a> {
    id: &'a str,
    project_id: &'a str,
    parent_id: Option<&'a str>,
    created: i64,
    updated: i64,
    shared: bool,
    compacting: bool,
    archived: bool,
}

fn insert_session(connection: &Connection, seed: &SessionSeed<'_>) {
    connection
        .execute(
            "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, \
             share_url, time_created, time_updated, time_compacting, time_archived) \
             VALUES (?1, ?2, ?3, ?1, '/srv', ?1, 'test', ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                seed.id,
                seed.project_id,
                seed.parent_id,
                seed.shared.then_some("https://share.example/session"),
                seed.created,
                seed.updated,
                seed.compacting.then_some(seed.updated),
                seed.archived.then_some(seed.updated),
            ],
        )
        .expect("insert session");
}

fn ids(report: &oc_db::retention::RetentionReport) -> BTreeSet<&str> {
    report
        .selected
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect()
}

fn all_projects() -> RetentionRequest {
    RetentionRequest::new(90, RetentionScope::AllProjects, NOW)
}

#[test]
fn retention_selects_the_whole_three_level_subtree() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    insert_session(
        &connection,
        &SessionSeed {
            id: "root",
            project_id: "prj_a",
            created: OLD,
            updated: OLD,
            ..SessionSeed::default()
        },
    );
    insert_session(
        &connection,
        &SessionSeed {
            id: "child",
            project_id: "prj_a",
            parent_id: Some("root"),
            created: NEW,
            updated: NEW,
            ..SessionSeed::default()
        },
    );
    insert_session(
        &connection,
        &SessionSeed {
            id: "grandchild",
            project_id: "prj_a",
            parent_id: Some("child"),
            created: NEW,
            updated: NEW,
            ..SessionSeed::default()
        },
    );

    let report = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
        .expect("select retention candidates");

    assert_eq!(
        ids(&report),
        BTreeSet::from(["child", "grandchild", "root"])
    );
    let grandchild = report
        .selected
        .iter()
        .find(|candidate| candidate.id == "grandchild")
        .expect("grandchild selected");
    assert!(grandchild.reasons.contains(&SelectionReason::DescendantOf {
        candidate_id: "root".to_owned(),
    }));
}

#[test]
fn retention_old_child_does_not_pull_in_its_newer_parent() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    insert_session(
        &connection,
        &SessionSeed {
            id: "parent",
            project_id: "prj_a",
            created: NEW,
            updated: NEW,
            ..SessionSeed::default()
        },
    );
    insert_session(
        &connection,
        &SessionSeed {
            id: "old-child",
            project_id: "prj_a",
            parent_id: Some("parent"),
            created: OLD,
            updated: OLD,
            ..SessionSeed::default()
        },
    );

    let report = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
        .expect("select retention candidates");
    assert_eq!(ids(&report), BTreeSet::from(["old-child"]));
}

#[test]
fn retention_shared_and_compacting_are_isolated_protections_with_overrides() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    insert_session(
        &connection,
        &SessionSeed {
            id: "shared",
            project_id: "prj_a",
            created: OLD,
            updated: OLD,
            shared: true,
            ..SessionSeed::default()
        },
    );
    insert_session(
        &connection,
        &SessionSeed {
            id: "compacting",
            project_id: "prj_a",
            created: OLD,
            updated: OLD,
            compacting: true,
            ..SessionSeed::default()
        },
    );

    let protected = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
        .expect("select protected sessions");
    assert!(protected.selected.is_empty());
    assert_eq!(protected.excluded.len(), 2);
    assert!(protected.excluded.iter().any(|item| {
        item.id == "shared"
            && item
                .reasons
                .contains(&ExclusionReason::Protected(ProtectionReason::Shared))
    }));
    assert!(protected.excluded.iter().any(|item| {
        item.id == "compacting"
            && item
                .reasons
                .contains(&ExclusionReason::Protected(ProtectionReason::Compacting))
    }));

    let included = select(
        &connection,
        &all_projects().including_shared().including_compacting(),
        &FakeProbe::reachable(&[]),
    )
    .expect("select with overrides");
    assert_eq!(ids(&included), BTreeSet::from(["compacting", "shared"]));
}

#[test]
fn retention_protected_descendant_vetoes_ancestor_subtree() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    insert_session(
        &connection,
        &SessionSeed {
            id: "root",
            project_id: "prj_a",
            created: OLD,
            updated: OLD,
            ..SessionSeed::default()
        },
    );
    insert_session(
        &connection,
        &SessionSeed {
            id: "shared-child",
            project_id: "prj_a",
            parent_id: Some("root"),
            created: NEW,
            updated: NEW,
            shared: true,
            ..SessionSeed::default()
        },
    );

    let protected = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
        .expect("select protected subtree");
    assert!(protected.selected.is_empty());
    assert_eq!(protected.excluded.len(), 1);
    assert!(
        protected.excluded[0]
            .reasons
            .contains(&ExclusionReason::ProtectedDescendant {
                descendant_id: "shared-child".to_owned(),
                protections: vec![ProtectionReason::Shared],
            })
    );

    let included = select(
        &connection,
        &all_projects().including_shared(),
        &FakeProbe::reachable(&[]),
    )
    .expect("select shared subtree with override");
    assert_eq!(ids(&included), BTreeSet::from(["root", "shared-child"]));
}

#[test]
fn retention_unreachable_server_uses_recency_guard_and_names_override() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    let five_minutes = 5 * 60 * 1_000;
    insert_session(
        &connection,
        &SessionSeed {
            id: "just-touched",
            project_id: "prj_a",
            created: OLD,
            updated: NOW - five_minutes,
            ..SessionSeed::default()
        },
    );
    let request = all_projects().created();

    let protected =
        select(&connection, &request, &FakeProbe::unreachable()).expect("recency fallback");
    assert!(protected.selected.is_empty());
    assert!(matches!(
        &protected.excluded[0].reasons[0],
        ExclusionReason::Protected(ProtectionReason::Recent { time_updated, .. })
            if *time_updated == NOW - five_minutes
    ));
    let rendered = format!(
        "{:?}; use --include-recent to override",
        protected.excluded[0]
    );
    assert!(rendered.contains("--include-recent"));

    let included = select(
        &connection,
        &request.including_recent(),
        &FakeProbe::unreachable(),
    )
    .expect("override recency fallback");
    assert_eq!(ids(&included), BTreeSet::from(["just-touched"]));
}

#[test]
fn retention_reachable_server_excludes_reported_active_ids_without_recency_fallback() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    for id in ["active", "idle"] {
        insert_session(
            &connection,
            &SessionSeed {
                id,
                project_id: "prj_a",
                created: OLD,
                updated: OLD,
                ..SessionSeed::default()
            },
        );
    }

    let report = select(
        &connection,
        &all_projects(),
        &FakeProbe::reachable(&["active"]),
    )
    .expect("probe active sessions");
    assert_eq!(ids(&report), BTreeSet::from(["idle"]));
    assert!(report.excluded.iter().any(|item| {
        item.id == "active"
            && item
                .reasons
                .contains(&ExclusionReason::Protected(ProtectionReason::Active))
    }));
}

#[test]
fn retention_archived_is_not_a_protection() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    insert_session(
        &connection,
        &SessionSeed {
            id: "archived",
            project_id: "prj_a",
            created: OLD,
            updated: OLD,
            archived: true,
            ..SessionSeed::default()
        },
    );

    let report = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
        .expect("select archived session");
    assert_eq!(ids(&report), BTreeSet::from(["archived"]));
}

#[test]
fn retention_scope_and_timestamp_key_are_explicit() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    insert_project(&connection, "prj_b");
    insert_session(
        &connection,
        &SessionSeed {
            id: "a-created-old",
            project_id: "prj_a",
            created: OLD,
            updated: NEW,
            ..SessionSeed::default()
        },
    );
    insert_session(
        &connection,
        &SessionSeed {
            id: "b-updated-old",
            project_id: "prj_b",
            created: OLD,
            updated: OLD,
            ..SessionSeed::default()
        },
    );

    let current = select(
        &connection,
        &RetentionRequest::new(90, RetentionScope::CurrentProject("prj_a".to_owned()), NOW)
            .created(),
        &FakeProbe::reachable(&[]),
    )
    .expect("select current project by creation");
    assert_eq!(ids(&current), BTreeSet::from(["a-created-old"]));

    let named = select(
        &connection,
        &RetentionRequest::new(90, RetentionScope::Project("prj_b".to_owned()), NOW),
        &FakeProbe::reachable(&[]),
    )
    .expect("select named project by update");
    assert_eq!(ids(&named), BTreeSet::from(["b-updated-old"]));

    let updated = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
        .expect("select all projects by update");
    assert_eq!(ids(&updated), BTreeSet::from(["b-updated-old"]));
}

#[test]
fn retention_age_boundary_is_strictly_older_than_cutoff() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    let cutoff = NOW - 90 * DAY_MILLIS;
    insert_session(
        &connection,
        &SessionSeed {
            id: "at-boundary",
            project_id: "prj_a",
            created: cutoff,
            updated: cutoff,
            ..SessionSeed::default()
        },
    );
    insert_session(
        &connection,
        &SessionSeed {
            id: "older-by-one",
            project_id: "prj_a",
            created: cutoff - 1,
            updated: cutoff - 1,
            ..SessionSeed::default()
        },
    );

    let report = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
        .expect("select around boundary");
    assert_eq!(report.age_cutoff_ms, cutoff);
    assert_eq!(ids(&report), BTreeSet::from(["older-by-one"]));
}

#[test]
fn retention_cycle_terminates_and_selects_each_row_once() {
    let pool = pool();
    let connection = pool.get().expect("connection");
    insert_project(&connection, "prj_a");
    for id in ["a", "b"] {
        insert_session(
            &connection,
            &SessionSeed {
                id,
                project_id: "prj_a",
                created: OLD,
                updated: OLD,
                ..SessionSeed::default()
            },
        );
    }
    connection
        .execute("UPDATE session SET parent_id = 'b' WHERE id = 'a'", [])
        .expect("link a to b");
    connection
        .execute("UPDATE session SET parent_id = 'a' WHERE id = 'b'", [])
        .expect("link b to a");

    let report = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
        .expect("select cyclic graph");
    assert_eq!(ids(&report), BTreeSet::from(["a", "b"]));
    assert_eq!(report.selected.len(), 2);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn retention_property_selected_forests_are_closed_under_descendants(
        nodes in prop::collection::vec((any::<u8>(), any::<bool>()), 2..20),
    ) {
        let pool = pool();
        let connection = pool.get().expect("connection");
        insert_project(&connection, "prj_property");
        let mut parents: BTreeMap<String, Option<String>> = BTreeMap::new();

        for (index, (selector, generated_old)) in nodes.iter().copied().enumerate() {
            let id = format!("node-{index:02}");
            let parent_index = if index == 0 {
                None
            } else if index == 1 {
                Some(0)
            } else {
                let choice = usize::from(selector) % (index + 1);
                (choice < index).then_some(choice)
            };
            let parent_id = parent_index.map(|parent| format!("node-{parent:02}"));
            let is_root = parent_id.is_none();
            let is_old = is_root || (index != 1 && generated_old);
            insert_session(
                &connection,
                &SessionSeed {
                    id: &id,
                    project_id: "prj_property",
                    parent_id: parent_id.as_deref(),
                    created: if is_old { OLD } else { NEW },
                    updated: if is_old { OLD } else { NEW },
                    ..SessionSeed::default()
                },
            );
            parents.insert(id, parent_id);
        }

        let report = select(&connection, &all_projects(), &FakeProbe::reachable(&[]))
            .expect("select random forest");
        let selected: BTreeSet<_> = report.selected.iter().map(|candidate| candidate.id.clone()).collect();

        prop_assert_eq!(selected.len(), nodes.len(), "every old root closes its whole tree");
        for (id, parent) in &parents {
            if let Some(parent) = parent {
                prop_assert!(
                    !selected.contains(parent) || selected.contains(id),
                    "selected parent {parent} stranded child {id}"
                );
            }
        }
    }
}

#[test]
fn retention_default_key_is_updated() {
    assert_eq!(RetentionKey::default(), RetentionKey::Updated);
}

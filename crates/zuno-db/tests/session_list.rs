//! The cross-project listing service: scope, archived semantics, ordering, and
//! the project summary.
//!
//! Every test here is named `session_list_*` so that `cargo test -p zuno-db
//! session_list` — the filter this module was specified with — actually selects
//! them rather than reporting a vacuous `0 passed; N filtered out`.
//!
//! Two of them exist to fail under a specific mutation rather than to describe
//! behaviour:
//!
//! * [`session_list_ties_are_broken_by_descending_id`] inserts eight sessions
//!   sharing one `time_updated` in **ascending** id order, so dropping the
//!   `id DESC` tie-break leaves SQLite free to return them in scan order and the
//!   assertion sees ascending ids.
//! * [`session_list_archived_widens_the_result_instead_of_replacing_it`] asserts
//!   the live sessions are still present, so redefining `--archived` as "only
//!   archived" fails on the live rows rather than on a count.

use std::collections::BTreeSet;

use zuno_db::session::{SessionCreate, SessionSort, Store};
use zuno_db::session_list::{
    GlobalListRequest, ProjectScope, list, message_counts, resolve_project,
};
use zuno_db::{Connection, Pool, migration, session_list};
use zuno_paths::DbLocation;

const VERSION: &str = "1.18.13";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn pool() -> Pool {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    {
        let mut connection = pool.get().expect("check out a connection");
        migration::apply(&mut connection).expect("apply the schema");
    }
    pool
}

fn insert_project(connection: &Connection, id: &str, worktree: &str, name: Option<&str>) {
    connection
        .execute(
            "INSERT INTO project (id, worktree, name, time_created, time_updated, sandboxes) \
             VALUES (?1, ?2, ?3, 1, 1, '[]')",
            rusqlite::params![id, worktree, name],
        )
        .expect("insert project");
}

fn insert_message(connection: &Connection, id: &str, session_id: &str) {
    connection
        .execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, 1, 1, '{}')",
            [id, session_id],
        )
        .expect("insert message");
}

struct Seed {
    id: &'static str,
    project: &'static str,
    worktree: &'static str,
    parent: Option<&'static str>,
    created: i64,
    updated: i64,
    archived: Option<i64>,
    agent: Option<&'static str>,
    cost: f64,
    messages: usize,
}

/// Three projects, each with a root session, two of them with a child, and one
/// archived root — the shape the acceptance criteria call for.
///
/// `created` and `updated` are deliberately anti-correlated across projects so
/// that the two sort orders produce genuinely different sequences; a fixture
/// where both orders agree cannot tell them apart.
const SEEDS: &[Seed] = &[
    Seed {
        id: "ses_one_root",
        project: "prj_one",
        worktree: "/srv/one",
        parent: None,
        created: 1_000,
        updated: 5_000,
        archived: None,
        agent: Some("build"),
        cost: 1.25,
        messages: 3,
    },
    Seed {
        id: "ses_one_kid",
        project: "prj_one",
        worktree: "/srv/one",
        parent: Some("ses_one_root"),
        created: 1_100,
        updated: 4_500,
        archived: None,
        agent: Some("plan"),
        cost: 0.5,
        messages: 1,
    },
    Seed {
        id: "ses_one_archived",
        project: "prj_one",
        worktree: "/srv/one",
        parent: None,
        created: 1_200,
        updated: 4_800,
        archived: Some(4_900),
        agent: None,
        cost: 0.0,
        messages: 0,
    },
    Seed {
        id: "ses_two_root",
        project: "prj_two",
        worktree: "/srv/two",
        parent: None,
        created: 3_000,
        updated: 4_000,
        archived: None,
        agent: Some("build"),
        cost: 2.0,
        messages: 2,
    },
    Seed {
        id: "ses_two_kid",
        project: "prj_two",
        worktree: "/srv/two",
        parent: Some("ses_two_root"),
        created: 3_100,
        updated: 3_500,
        archived: None,
        agent: None,
        cost: 0.0,
        messages: 0,
    },
    Seed {
        id: "ses_three_root",
        project: "prj_three",
        worktree: "/srv/three",
        parent: None,
        created: 4_000,
        updated: 3_000,
        archived: None,
        agent: Some("general"),
        cost: 0.125,
        messages: 5,
    },
];

fn seeded() -> Pool {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_one", "/srv/one", Some("One"));
        insert_project(&connection, "prj_two", "/srv/two", None);
        insert_project(&connection, "prj_three", "/srv/three", Some("Three"));
    }
    let store = Store::new(&pool);
    for seed in SEEDS {
        let mut input = SessionCreate::new(
            seed.id,
            seed.id.trim_start_matches("ses_"),
            seed.project,
            seed.worktree,
            seed.worktree,
            format!("Title for {}", seed.id),
            VERSION,
        )
        .at(seed.created);
        input.agent = seed.agent.map(str::to_owned);
        if let Some(parent) = seed.parent {
            input = input.with_parent(parent);
        }
        store.create(&input).expect("create session");
        store
            .touch_at(seed.id, seed.updated)
            .expect("set last activity");

        let connection = pool.get().expect("check out a connection");
        connection
            .execute(
                "UPDATE session SET time_archived = ?2, cost = ?3 WHERE id = ?1",
                rusqlite::params![seed.id, seed.archived, seed.cost],
            )
            .expect("apply archive marker and cost");
        for index in 0..seed.messages {
            insert_message(&connection, &format!("msg_{}_{index}", seed.id), seed.id);
        }
    }
    pool
}

fn ids(listed: &[session_list::ListedSession]) -> Vec<String> {
    listed
        .iter()
        .map(|entry| entry.info.session.id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

#[test]
fn session_list_all_projects_returns_every_root_with_its_project_summary() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let listed = list(&connection, &GlobalListRequest::all_projects().roots_only())
        .expect("list every project");

    assert_eq!(
        ids(&listed),
        vec![
            String::from("ses_one_root"),
            String::from("ses_two_root"),
            String::from("ses_three_root"),
        ],
        "every live root session, newest activity first"
    );

    let projects: Vec<Option<(String, Option<String>, String)>> = listed
        .iter()
        .map(|entry| {
            entry.info.project.as_ref().map(|project| {
                (
                    project.id.clone(),
                    project.name.clone(),
                    project.worktree.clone(),
                )
            })
        })
        .collect();
    assert_eq!(
        projects,
        vec![
            Some((
                String::from("prj_one"),
                Some(String::from("One")),
                String::from("/srv/one")
            )),
            Some((String::from("prj_two"), None, String::from("/srv/two"))),
            Some((
                String::from("prj_three"),
                Some(String::from("Three")),
                String::from("/srv/three")
            )),
        ],
        "each row carries the project that owns it, name included when set"
    );

    let distinct: BTreeSet<&str> = listed
        .iter()
        .map(|entry| entry.info.session.project_id.as_str())
        .collect();
    assert_eq!(distinct.len(), 3, "the listing spans all three projects");
}

#[test]
fn session_list_all_projects_includes_children_unless_roots_is_asked_for() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let everything =
        list(&connection, &GlobalListRequest::all_projects()).expect("list without roots");
    assert_eq!(
        ids(&everything),
        vec![
            String::from("ses_one_root"),
            String::from("ses_one_kid"),
            String::from("ses_two_root"),
            String::from("ses_two_kid"),
            String::from("ses_three_root"),
        ]
    );
    let kid = everything
        .iter()
        .find(|entry| entry.info.session.id == "ses_one_kid")
        .expect("the child session");
    assert_eq!(kid.info.session.parent_id.as_deref(), Some("ses_one_root"));
}

#[test]
fn session_list_one_project_narrows_to_that_project_only() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    for (project, expected) in [
        ("prj_one", vec!["ses_one_root", "ses_one_kid"]),
        ("prj_two", vec!["ses_two_root", "ses_two_kid"]),
        ("prj_three", vec!["ses_three_root"]),
    ] {
        let listed =
            list(&connection, &GlobalListRequest::project(project)).expect("list one project");
        assert_eq!(ids(&listed), expected, "{project}");
        assert!(
            listed
                .iter()
                .all(|entry| entry.info.session.project_id == project),
            "{project} leaked a foreign session"
        );
    }
}

#[test]
fn session_list_an_unknown_project_lists_nothing_rather_than_everything() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let listed = list(&connection, &GlobalListRequest::project("prj_absent"))
        .expect("list a missing project");
    assert!(listed.is_empty(), "{:?}", ids(&listed));
}

#[test]
fn session_list_never_injects_an_ambient_project() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let listed = list(&connection, &GlobalListRequest::all_projects()).expect("list every project");
    let scoped =
        list(&connection, &GlobalListRequest::project("prj_one")).expect("list one project");
    assert!(
        listed.len() > scoped.len(),
        "a global listing that matched a single project's would mean a project id was injected: \
         global {} vs prj_one {}",
        listed.len(),
        scoped.len()
    );
    assert_eq!(
        ProjectScope::AllProjects,
        GlobalListRequest::default().scope
    );
}

// ---------------------------------------------------------------------------
// Archived
// ---------------------------------------------------------------------------

#[test]
fn session_list_hides_archived_sessions_by_default() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let listed = list(&connection, &GlobalListRequest::all_projects()).expect("default listing");
    assert!(
        !ids(&listed).contains(&String::from("ses_one_archived")),
        "{:?}",
        ids(&listed)
    );
    assert!(
        listed
            .iter()
            .all(|entry| entry.info.session.time.archived.is_none())
    );
}

#[test]
fn session_list_archived_widens_the_result_instead_of_replacing_it() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let live = ids(&list(&connection, &GlobalListRequest::all_projects()).expect("live listing"));
    let widened = ids(&list(
        &connection,
        &GlobalListRequest::all_projects().including_archived(),
    )
    .expect("widened listing"));

    for id in &live {
        assert!(
            widened.contains(id),
            "--archived dropped the live session {id}; it must ADD archived sessions, not select \
             them exclusively. widened = {widened:?}"
        );
    }
    assert!(
        widened.contains(&String::from("ses_one_archived")),
        "{widened:?}"
    );
    assert_eq!(
        widened.len(),
        live.len() + 1,
        "exactly one archived session joins the live ones"
    );
    assert_eq!(
        widened,
        vec![
            String::from("ses_one_root"),
            String::from("ses_one_archived"),
            String::from("ses_one_kid"),
            String::from("ses_two_root"),
            String::from("ses_two_kid"),
            String::from("ses_three_root"),
        ],
        "the archived session takes its place in activity order, not a separate section"
    );
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

#[test]
fn session_list_honours_both_sort_orders() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");

    let updated = ids(
        &list(&connection, &GlobalListRequest::all_projects().roots_only())
            .expect("sort by last activity"),
    );
    let created = ids(&list(
        &connection,
        &GlobalListRequest::all_projects()
            .roots_only()
            .created_order(),
    )
    .expect("sort by creation"));

    assert_eq!(
        updated,
        vec![
            String::from("ses_one_root"),
            String::from("ses_two_root"),
            String::from("ses_three_root"),
        ]
    );
    assert_eq!(
        created,
        vec![
            String::from("ses_three_root"),
            String::from("ses_two_root"),
            String::from("ses_one_root"),
        ]
    );
    assert_ne!(
        updated, created,
        "the fixture must distinguish the two orders or neither is tested"
    );
    assert_eq!(GlobalListRequest::default().sort, SessionSort::Updated);
}

#[test]
fn session_list_ties_are_broken_by_descending_id() {
    let pool = pool();
    {
        let connection = pool.get().expect("check out a connection");
        insert_project(&connection, "prj_tie", "/srv/tie", Some("Tie"));
    }
    let store = Store::new(&pool);
    let inserted: Vec<String> = (1..=8).map(|index| format!("ses_tie_{index:02}")).collect();
    for id in &inserted {
        store
            .create(
                &SessionCreate::new(id, id, "prj_tie", "/srv/tie", "/srv/tie", id, VERSION)
                    .at(7_000),
            )
            .expect("create session");
    }

    let connection = pool.get().expect("check out a connection");
    let listed = ids(&list(&connection, &GlobalListRequest::all_projects()).expect("list ties"));

    let mut expected = inserted.clone();
    expected.sort_by(|left, right| right.cmp(left));
    assert_eq!(
        listed, expected,
        "eight sessions sharing one time_updated must come back in descending id order; \
         inserted ascending as {inserted:?}"
    );

    let again = ids(&list(&connection, &GlobalListRequest::all_projects()).expect("list ties"));
    assert_eq!(listed, again, "the order must be stable across reads");
}

// ---------------------------------------------------------------------------
// Limit
// ---------------------------------------------------------------------------

#[test]
fn session_list_limit_truncates_the_head_of_the_order() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let listed = ids(&list(
        &connection,
        &GlobalListRequest::all_projects().with_limit(2),
    )
    .expect("limited listing"));
    assert_eq!(
        listed,
        vec![String::from("ses_one_root"), String::from("ses_one_kid")]
    );
}

#[test]
fn session_list_limit_above_the_upstream_default_is_honoured() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let request = GlobalListRequest::all_projects().with_limit(500);
    assert_eq!(request.effective_limit(), 500);
    let listed = list(&connection, &request).expect("large limit");
    assert_eq!(listed.len(), 5);
}

// ---------------------------------------------------------------------------
// The table's aggregates
// ---------------------------------------------------------------------------

#[test]
fn session_list_carries_the_message_count_and_the_row_cost() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let listed = list(&connection, &GlobalListRequest::all_projects()).expect("list every project");

    let observed: Vec<(String, i64, f64)> = listed
        .iter()
        .map(|entry| {
            (
                entry.info.session.id.clone(),
                entry.messages,
                entry.info.session.cost,
            )
        })
        .collect();
    assert_eq!(
        observed,
        vec![
            (String::from("ses_one_root"), 3, 1.25),
            (String::from("ses_one_kid"), 1, 0.5),
            (String::from("ses_two_root"), 2, 2.0),
            (String::from("ses_two_kid"), 0, 0.0),
            (String::from("ses_three_root"), 5, 0.125),
        ]
    );
}

#[test]
fn session_list_message_counts_agree_with_the_composed_query() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let listed = list(&connection, &GlobalListRequest::all_projects()).expect("list every project");
    let session_ids: Vec<String> = ids(&listed);
    let counts = message_counts(&connection, &session_ids).expect("count messages");

    for entry in &listed {
        let separate = counts.get(&entry.info.session.id).copied().unwrap_or(0);
        assert_eq!(
            entry.messages, separate,
            "{} disagreed between the joined count and the grouped one",
            entry.info.session.id
        );
    }
    assert!(
        message_counts(&connection, &[])
            .expect("count nothing")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// The project summary's edges
// ---------------------------------------------------------------------------

#[test]
fn session_list_reports_a_null_project_when_the_project_row_is_gone() {
    let pool = pool();
    let connection = pool.get().expect("check out a connection");
    connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .expect("suspend foreign keys");
    connection
        .execute(
            "INSERT INTO session (id, project_id, slug, directory, path, title, version, cost, \
             tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, \
             tokens_cache_write, time_created, time_updated) \
             VALUES ('ses_stray', 'prj_deleted', 'stray', '/srv/gone', '', 'Stray', ?1, 0, 0, 0, \
             0, 0, 0, 10, 10)",
            [VERSION],
        )
        .expect("insert a session whose project is absent");
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .expect("restore foreign keys");

    let listed = list(&connection, &GlobalListRequest::all_projects()).expect("list every project");
    assert_eq!(ids(&listed), vec![String::from("ses_stray")]);
    assert!(
        listed[0].info.project.is_none(),
        "a session whose project row is gone must still list, with a null project"
    );
}

#[test]
fn session_list_resolves_a_project_by_id_and_by_worktree_path() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");

    let by_id = resolve_project(&connection, "prj_two")
        .expect("resolve by id")
        .expect("prj_two exists");
    assert_eq!(by_id.id, "prj_two");
    assert_eq!(by_id.worktree, "/srv/two");
    assert_eq!(by_id.name, None);

    let by_path = resolve_project(&connection, "/srv/three")
        .expect("resolve by worktree")
        .expect("/srv/three exists");
    assert_eq!(by_path.id, "prj_three");
    assert_eq!(by_path.name.as_deref(), Some("Three"));

    assert!(
        resolve_project(&connection, "prj_absent")
            .expect("resolve a missing project")
            .is_none()
    );
    assert!(
        resolve_project(&connection, "/srv/absent")
            .expect("resolve a missing worktree")
            .is_none()
    );
}

#[test]
fn session_list_lists_only_projects_that_own_sessions() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    insert_project(&connection, "prj_empty", "/srv/empty", Some("Empty"));
    let projects = session_list::projects_with_sessions(&connection).expect("list projects");
    let observed: Vec<&str> = projects.iter().map(|project| project.id.as_str()).collect();
    assert_eq!(observed, vec!["prj_one", "prj_three", "prj_two"]);
}

// ---------------------------------------------------------------------------
// The wire shape
// ---------------------------------------------------------------------------

#[test]
fn session_list_serialises_the_upstream_global_info_shape() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    let listed = list(
        &connection,
        &GlobalListRequest::project("prj_one").roots_only(),
    )
    .expect("list one project");
    let json = session_list::to_json(&listed).expect("serialise");
    let rows = json.as_array().expect("an array of sessions");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];

    assert_eq!(row["id"], "ses_one_root");
    assert_eq!(row["projectID"], "prj_one");
    assert_eq!(row["slug"], "one_root");
    assert_eq!(row["directory"], "/srv/one");
    assert_eq!(row["path"], "");
    assert_eq!(row["title"], "Title for ses_one_root");
    assert_eq!(row["agent"], "build");
    assert_eq!(row["version"], VERSION);
    assert_eq!(row["cost"], 1.25);
    assert_eq!(row["tokens"]["input"], 0);
    assert_eq!(row["tokens"]["cache"]["read"], 0);
    assert_eq!(row["time"]["created"], 1_000);
    assert_eq!(row["time"]["updated"], 5_000);
    assert_eq!(row["project"]["id"], "prj_one");
    assert_eq!(row["project"]["name"], "One");
    assert_eq!(row["project"]["worktree"], "/srv/one");

    let object = row.as_object().expect("a session object");
    for absent in [
        "workspaceID",
        "parentID",
        "model",
        "summary",
        "share",
        "metadata",
        "revert",
        "permission",
    ] {
        assert!(
            !object.contains_key(absent),
            "{absent} must be omitted rather than emitted as null"
        );
    }
    assert!(
        !object.contains_key("messages"),
        "the message count must stay out of the endpoint's shape"
    );
    assert!(
        !row["time"]
            .as_object()
            .expect("a time object")
            .contains_key("archived")
    );

    let widened = list(
        &connection,
        &GlobalListRequest::project("prj_one")
            .roots_only()
            .including_archived(),
    )
    .expect("list with archived");
    let archived = session_list::to_json(&widened).expect("serialise");
    let marked = archived
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == "ses_one_archived")
        .expect("the archived session");
    assert_eq!(marked["time"]["archived"], 4_900);

    let missing_name = session_list::to_json(
        &list(&connection, &GlobalListRequest::project("prj_two")).expect("list prj_two"),
    )
    .expect("serialise");
    let two = &missing_name.as_array().expect("an array")[0];
    assert!(
        !two["project"]
            .as_object()
            .expect("a project object")
            .contains_key("name"),
        "an unnamed project omits `name`, matching `name ?? undefined`"
    );
}

#[test]
fn session_list_reveals_json_columns_instead_of_quoting_them() {
    let pool = seeded();
    let connection = pool.get().expect("check out a connection");
    connection
        .execute(
            "UPDATE session SET model = ?2, metadata = ?3, permission = ?4, \
             summary_additions = 4, summary_deletions = 5, summary_files = 6, \
             summary_diffs = ?5, share_url = ?6, workspace_id = ?7, time_compacting = 42 \
             WHERE id = ?1",
            rusqlite::params![
                "ses_three_root",
                r#"{"id":"claude","providerID":"anthropic"}"#,
                r#"{"source":"cli"}"#,
                r#"[{"action":"edit"}]"#,
                r#"["a.rs"]"#,
                "https://share.example/abc",
                "wrk_one",
            ],
        )
        .expect("populate the opaque columns");

    let listed =
        list(&connection, &GlobalListRequest::project("prj_three")).expect("list prj_three");
    let json = session_list::to_json(&listed).expect("serialise");
    let row = &json.as_array().expect("an array")[0];

    assert_eq!(row["model"]["providerID"], "anthropic");
    assert_eq!(row["metadata"]["source"], "cli");
    assert_eq!(row["permission"][0]["action"], "edit");
    assert_eq!(row["summary"]["additions"], 4);
    assert_eq!(row["summary"]["files"], 6);
    assert_eq!(row["summary"]["diffs"][0], "a.rs");
    assert_eq!(row["share"]["url"], "https://share.example/abc");
    assert_eq!(row["workspaceID"], "wrk_one");
    assert_eq!(row["time"]["compacting"], 42);
}

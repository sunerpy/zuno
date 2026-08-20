//! What delegation must do against a real database.
//!
//! These exercise the three things a [`ChildTurnHost`] owes
//! [`zuno_tools::task::TaskTool`] and that no test double can prove: the ancestry walk
//! the depth guard reads, the child session row a delegation creates, and the resume
//! check that keeps one parent out of another's children. Driving a full child turn
//! needs a provider, so that is covered by the registry membership assertions and the
//! real-binary check instead.

use super::*;
use crate::command::GlobalOptions;
use zuno_paths::{DbLocation, Env};

struct Fixture {
    _root: tempfile::TempDir,
    host: ChildSessionHost,
    database: DbLocation,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::TempDir::new().expect("temporary delegation root");
        let database = DbLocation::File(root.path().join("delegation.db"));
        let mut connection = zuno_db::open::open(&database).expect("open the fixture database");
        zuno_db::migration::apply(&mut connection).expect("migrate the fixture database");
        drop(connection);

        let environment = StartupEnvironment::resolve(&Env::empty(), &GlobalOptions::default());
        let mut host = ChildSessionHost::new(
            environment,
            root.path().to_path_buf(),
            Arc::new(zuno_tool::AllowAll),
        );
        host.database = database.clone();
        Self {
            _root: root,
            host,
            database,
        }
    }

    fn connection(&self) -> rusqlite::Connection {
        zuno_db::open::open(&self.database).expect("reopen the fixture database")
    }

    /// A session row, optionally a child of `parent`.
    fn session(&self, id: &str, parent: Option<&str>) {
        let mut connection = self.connection();
        connection
            .execute(
                "INSERT INTO project (id, worktree, vcs, time_created, time_updated, sandboxes) \
                 VALUES ('proj', '/tmp/proj', NULL, 1, 1, '[]') ON CONFLICT (id) DO NOTHING",
                (),
            )
            .expect("seed the project row");
        let mut input = zuno_db::session::SessionCreate::new(
            id,
            id,
            "proj",
            "/tmp/proj",
            "/tmp/proj",
            "fixture session",
            crate::COMPATIBILITY_VERSION,
        )
        .at(1);
        if let Some(parent) = parent {
            input = input.with_parent(parent);
        }
        let transaction = connection.transaction().expect("begin");
        zuno_db::session::create(&transaction, &input).expect("create the session row");
        transaction.commit().expect("commit");
    }

    fn request(&self, parent: &str) -> ChildTurnRequest {
        ChildTurnRequest {
            parent_session_id: parent.to_owned(),
            resume_session_id: None,
            agent: "worker".to_owned(),
            description: Some("a delegated unit".to_owned()),
            prompt: "do the thing".to_owned(),
            model: None,
            effort: None,
            provider_options: serde_json::Map::new(),
            background: false,
        }
    }
}

/// The measure the depth guard reads must come from the real `parent_id` chain.
///
/// `TaskTool` refuses at `depth >= subagent_depth`, so these exact numbers are what
/// makes the default one-hop bound one hop. A host answering `0` for everything would
/// pass every `zuno-tools` test and allow unbounded delegation in production.
#[tokio::test]
async fn delegation_depth_counts_real_parent_hops() {
    let fixture = Fixture::new();
    fixture.session("ses_root", None);
    fixture.session("ses_child", Some("ses_root"));
    fixture.session("ses_grandchild", Some("ses_child"));

    for (session, expected) in [("ses_root", 0), ("ses_child", 1), ("ses_grandchild", 2)] {
        assert_eq!(
            fixture
                .host
                .delegation_depth(session)
                .await
                .expect("the ancestry walk reads the real chain"),
            expected,
            "{session}"
        );
    }
}

/// `parent_id` has no foreign key, so a cycle is representable and must terminate.
#[tokio::test]
async fn a_cyclic_parent_chain_is_refused_rather_than_walked_forever() {
    let fixture = Fixture::new();
    fixture.session("ses_a", None);
    fixture.session("ses_b", Some("ses_a"));
    fixture
        .connection()
        .execute(
            "UPDATE session SET parent_id = 'ses_b' WHERE id = 'ses_a'",
            (),
        )
        .expect("forge a cycle");

    let error = fixture
        .host
        .delegation_depth("ses_a")
        .await
        .expect_err("a cycle cannot yield a depth");

    assert!(format!("{error}").contains("cycle"), "{error}");
}

/// A delegation creates a child row the parent owns, carrying the requested agent.
///
/// The `parent_id` is what makes the depth guard work on the next hop, and the title
/// is what makes a delegated session findable; both are set here or nowhere.
#[tokio::test]
async fn a_fresh_delegation_creates_a_child_session_owned_by_its_parent() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);

    let child = fixture
        .host
        .session_for(&fixture.request("ses_owner"))
        .expect("a fresh delegation creates a child");

    let stored = zuno_db::session::get(&fixture.connection(), &child).expect("the child persisted");
    assert_eq!(stored.parent_id.as_deref(), Some("ses_owner"));
    assert_eq!(stored.agent.as_deref(), Some("worker"));
    assert_eq!(stored.title, "a delegated unit");
    assert_eq!(
        fixture
            .host
            .delegation_depth(&child)
            .await
            .expect("the child's own depth is readable"),
        1,
        "the row a delegation writes must be the row the depth guard then reads"
    );
}

/// `task_id` must name a child of *this* session, not any session at all.
///
/// Accepting a stranger's child would let one delegation continue a session its caller
/// was never handed, which is both a confusing transcript and a write into someone
/// else's history.
#[test]
fn resuming_another_parents_child_is_refused_by_name() {
    let fixture = Fixture::new();
    fixture.session("ses_mine", None);
    fixture.session("ses_theirs", None);
    fixture.session("ses_their_child", Some("ses_theirs"));

    let mut request = fixture.request("ses_mine");
    request.resume_session_id = Some("ses_their_child".to_owned());
    let error = fixture
        .host
        .session_for(&request)
        .expect_err("another parent's child is not resumable here");

    assert!(
        matches!(error, ChildTurnError::UnknownSession(ref id) if id == "ses_their_child"),
        "{error}"
    );
    assert!(
        format!("{error}").contains("drop `task_id`"),
        "the refusal must name the fix: {error}"
    );
}

#[test]
fn resuming_an_absent_session_is_refused_rather_than_silently_creating_one() {
    let fixture = Fixture::new();
    fixture.session("ses_mine", None);

    let mut request = fixture.request("ses_mine");
    request.resume_session_id = Some("ses_never_existed".to_owned());
    let error = fixture
        .host
        .session_for(&request)
        .expect_err("an absent session is not resumable");

    assert!(
        matches!(error, ChildTurnError::UnknownSession(_)),
        "{error}"
    );
}

#[test]
fn resuming_this_parents_own_child_reuses_it_rather_than_forking() {
    let fixture = Fixture::new();
    fixture.session("ses_mine", None);
    fixture.session("ses_my_child", Some("ses_mine"));

    let mut request = fixture.request("ses_mine");
    request.resume_session_id = Some("ses_my_child".to_owned());

    assert_eq!(
        fixture
            .host
            .session_for(&request)
            .expect("this parent's own child resumes"),
        "ses_my_child"
    );
}

/// Background dispatch must refuse, not hand back an id nobody can resolve.
///
/// The tool's own description promises the caller is notified on completion. Nothing
/// in this build reports a finished job, so a job id would name work that can never be
/// collected — a silent failure, where a refusal is one the model can act on.
#[tokio::test]
async fn background_delegation_is_refused_with_the_reason_and_the_alternative() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let error = fixture
        .host
        .dispatch(request)
        .await
        .expect_err("background delegation is not available");

    let rendered = format!("{error}");
    assert!(rendered.contains("background"), "{rendered}");
    assert!(
        rendered.contains("Drop `background`"),
        "the refusal must name the alternative: {rendered}"
    );
    assert_eq!(
        zuno_db::session::children(&fixture.connection(), "ses_owner")
            .expect("read children")
            .len(),
        0,
        "a refused delegation must not leave a child session behind"
    );
}

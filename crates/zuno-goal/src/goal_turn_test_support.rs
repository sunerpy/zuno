use crate::{GoalStore, GoalTurnIdentity};
use rusqlite::params;
use std::sync::Arc;
use zuno_tool::ToolContext;

pub(crate) fn select_cycle(store: &GoalStore, session: &str, cycle: &str, goal: Option<&str>) {
    store.pool().try_transaction(|tx| -> Result<(), crate::GoalError> {
        tx.execute(
            "INSERT OR IGNORE INTO project \
             (id,worktree,time_created,time_updated,sandboxes) VALUES ('scoped-prj','/tmp',1,1,'[]')", [],
        ).unwrap();
        tx.execute(
            "INSERT OR IGNORE INTO session \
             (id,project_id,slug,directory,title,version,time_created,time_updated) \
             VALUES (?1,'scoped-prj',?1,'/tmp',?1,'test',1,1)", [session],
        ).unwrap();
        let mut state = zuno_db::session_execution::seed_in(
            tx, session, zuno_types::execution::CollaborationMode::Work, None, 1,
        )?;
        state.cycle_id = Some(cycle.to_owned());
        zuno_db::session_execution::update_in(tx, state.revision, state)?;
        zuno_db::session_work_cycle::save_in(tx, &zuno_db::session_work_cycle::SessionWorkCycle {
            session_id: session.to_owned(), cycle_id: cycle.to_owned(),
            anchor_message_id: None, goal_id: goal.map(str::to_owned), plan_id: None,
            active_turn_id: None,
            todo_ids: Default::default(), resumed_goal_cycles: Default::default(),
            stopped: None, scheduling: None,
        }, 1)?;
        Ok(())
    }).unwrap();
}

pub(crate) fn bind(store: &GoalStore, session: &str, cycle: &str, turn: &str) -> GoalTurnIdentity {
    let goal = store.goal(session).unwrap().unwrap();
    select_cycle(store, session, cycle, Some(&goal.goal_id));
    let id = GoalTurnIdentity::new(goal.goal_id, cycle, turn).unwrap();
    let previous = store.current_goal_turn(session).unwrap();
    store
        .bind_goal_turn_checked(session, &id, previous.as_ref())
        .unwrap();
    id
}

/// Simulate the host's actual-engine-turn fence, with or without a preexisting Goal.
pub(crate) fn fence(store: &GoalStore, session: &str, turn: &str) {
    store
        .pool()
        .try_transaction(|tx| -> Result<(), crate::GoalError> {
            let mut scope = zuno_db::session_work_cycle::current_in(tx, session)?.unwrap();
            scope.active_turn_id = Some(turn.to_owned());
            zuno_db::session_work_cycle::save_in(tx, &scope, 1)?;
            Ok(())
        })
        .unwrap();
}

pub(crate) fn with_snapshot(context: ToolContext, identity: &GoalTurnIdentity) -> ToolContext {
    let snapshot = serde_json::from_value(serde_json::json!({
        "schemaVersion": 4,
        "turnId": identity.turn_id,
        "cycleId": identity.cycle_id,
        "step": 1,
        "capability": {
            "schemaVersion": 4,
            "pack": {"id":"test","version":"1","upstreamRevision":"test"},
            "extensionRevision": 0, "permissionPolicySha256": "policy",
            "sandbox": {"mode":"workspace-write","network":"deny","writableRoots":[],"protectedPaths":[]},
            "profiles": [], "presets": [], "councils": [], "workflows": [], "skills": []
        },
        "owner": {
            "sessionId": context.session_id, "parentSessionId":null, "parentAttempt":null,
            "workflow":null, "workflowNode":null
        },
        "agent": {
            "name":"build", "sourceId":"test://build", "definitionSha256":"definition",
            "permissionSha256":"permission", "promptPolicySha256":"prompt"
        },
        "model": {
            "providerId":"fake", "modelId":"fake-model", "wireModelId":"fake-model",
            "surface":"responses", "reasoningSha256":"reasoning", "preset":null
        },
        "selectedSkills": [], "tools": [],
        "prompt": {"eventId":"evt-parent","assemblySha256":"assembly","actualSha256":"actual"}
    })).unwrap();
    context.with_orchestration_snapshot(Arc::new(snapshot))
}

pub(crate) fn row_count(store: &GoalStore, table: &str, session: &str) -> i64 {
    store
        .pool()
        .get()
        .unwrap()
        .query_row(
            &format!("SELECT count(*) FROM {table} WHERE session_id=?1"),
            params![session],
            |row| row.get(0),
        )
        .unwrap()
}

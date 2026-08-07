use oc_db::session::Store;

use crate::command::{SessionArgs, SessionCommand};

pub(super) fn execute(args: &SessionArgs) -> Result<(), String> {
    let pool = open_store()?;
    match args
        .command
        .as_ref()
        .ok_or("session subcommand is required")?
    {
        SessionCommand::List(args) => super::session_list::run(&pool, args),
        SessionCommand::Prune(args) => super::session_prune::run(&pool, args),
        SessionCommand::Delete { session_id } => {
            let store = Store::new(&pool);
            if store
                .find(session_id)
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Err(format!("Session not found: {session_id}"));
            }
            store
                .remove(session_id)
                .map_err(|error| error.to_string())?;
            println!("Session {session_id} deleted");
            Ok(())
        }
    }
}

fn open_store() -> Result<oc_db::Pool, String> {
    let pool = oc_db::Pool::open_default().map_err(|error| error.to_string())?;
    let mut connection = pool.get().map_err(|error| error.to_string())?;
    oc_db::migration::apply(&mut connection).map_err(|error| error.to_string())?;
    drop(connection);
    Ok(pool)
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_scope_uses_the_current_checkout() {
        let project =
            oc_paths::project::resolve_project(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(!project.id.is_empty());
    }
}

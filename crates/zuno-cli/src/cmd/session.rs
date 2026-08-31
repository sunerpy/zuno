use zuno_db::session::Store;

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
        SessionCommand::Delete {
            session_id,
            keep_derived_experiences,
            cleanup_derived_experiences,
        } => {
            if !keep_derived_experiences && !cleanup_derived_experiences {
                return Err(
                    "choose exactly one of --keep-derived-experiences or --cleanup-derived-experiences"
                        .to_owned(),
                );
            }
            let store = Store::new(&pool);
            if store
                .find(session_id)
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Err(format!("Session not found: {session_id}"));
            }
            if *cleanup_derived_experiences {
                let experiences =
                    zuno_db::experience::ExperienceStore::new(std::sync::Arc::clone(&pool));
                let mut count = 0_usize;
                for id in store
                    .subtree(session_id)
                    .map_err(|error| error.to_string())?
                {
                    count = count.saturating_add(
                        experiences
                            .list_for_session(&id)
                            .map_err(|error| error.to_string())?
                            .len(),
                    );
                }
                if count > 0 {
                    return Err(
                        "derived-learning cleanup needs a live learning profile to prepare reviewed Memory and Skill revocations; delete from the TUI and choose `clean learning`, or use ACP with cleanupDerivedExperiences=true"
                            .to_owned(),
                    );
                }
            }
            store
                .remove(session_id)
                .map_err(|error| error.to_string())?;
            println!("Session {session_id} deleted");
            Ok(())
        }
    }
}

fn open_store() -> Result<std::sync::Arc<zuno_db::Pool>, String> {
    let pool =
        std::sync::Arc::new(zuno_db::Pool::open_default().map_err(|error| error.to_string())?);
    let mut connection = pool.get().map_err(|error| error.to_string())?;
    zuno_db::migration::apply(&mut connection).map_err(|error| error.to_string())?;
    drop(connection);
    Ok(pool)
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_scope_uses_the_current_checkout() {
        let project =
            zuno_paths::project::resolve_project(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(!project.id.is_empty());
    }
}

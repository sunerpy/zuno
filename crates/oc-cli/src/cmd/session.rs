use oc_db::session::{ListQuery, Session, Store, UPSTREAM_LIST_LIMIT};
use serde_json::json;
use time::{OffsetDateTime, UtcOffset};

use crate::command::{SessionArgs, SessionCommand, SessionFormat};

pub(super) fn execute(args: &SessionArgs) -> Result<(), String> {
    let pool = open_store()?;
    let store = Store::new(&pool);
    match args
        .command
        .as_ref()
        .ok_or("session subcommand is required")?
    {
        SessionCommand::List(args) => {
            let directory = std::env::current_dir().map_err(|error| error.to_string())?;
            let project = oc_paths::project::resolve_project(&directory);
            let mut query = ListQuery::project(project.id);
            query.roots = true;
            query.limit = Some(args.max_count.unwrap_or(UPSTREAM_LIST_LIMIT));
            let sessions = store.list(&query).map_err(|error| error.to_string())?;
            render_list(&sessions, args.format)
        }
        SessionCommand::Delete { session_id } => {
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

fn render_list(sessions: &[Session], format: SessionFormat) -> Result<(), String> {
    if sessions.is_empty() {
        return Ok(());
    }
    match format {
        SessionFormat::Json => {
            let rows: Vec<_> = sessions
                .iter()
                .map(|session| {
                    json!({
                        "id": session.id,
                        "title": session.title,
                        "updated": session.time_updated,
                        "created": session.time_created,
                        "projectId": session.project_id,
                        "directory": session.directory,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&rows).map_err(|error| error.to_string())?
            );
        }
        SessionFormat::Table => render_table(sessions)?,
    }
    Ok(())
}

fn render_table(sessions: &[Session]) -> Result<(), String> {
    let id_width = sessions
        .iter()
        .map(|session| session.id.len())
        .max()
        .unwrap_or(0)
        .max(20);
    let title_width = sessions
        .iter()
        .map(|session| utf16_len(&session.title))
        .max()
        .unwrap_or(0)
        .max(25);
    let header = format!(
        "Session ID{}  Title{}  Updated",
        " ".repeat(id_width - 10),
        " ".repeat(title_width - 5)
    );
    println!("{header}");
    println!("{}", "─".repeat(header.chars().count()));
    for session in sessions {
        let title = truncate_utf16(&session.title, title_width);
        println!(
            "{}{spaces_id}  {title}{spaces_title}  {}",
            session.id,
            today_time_or_date_time(session.time_updated)?,
            spaces_id = " ".repeat(id_width.saturating_sub(session.id.len())),
            spaces_title = " ".repeat(title_width.saturating_sub(utf16_len(&title))),
        );
    }
    Ok(())
}

fn today_time_or_date_time(milliseconds: i64) -> Result<String, String> {
    let seconds = milliseconds.div_euclid(1_000);
    let nanos = u32::try_from(milliseconds.rem_euclid(1_000)).map_err(|error| error.to_string())?
        * 1_000_000;
    let utc = OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|error| error.to_string())?
        .replace_nanosecond(nanos)
        .map_err(|error| error.to_string())?;
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let date = utc.to_offset(offset);
    let now = OffsetDateTime::now_utc().to_offset(offset);
    let hour = match date.hour() % 12 {
        0 => 12,
        value => value,
    };
    let suffix = if date.hour() < 12 { "AM" } else { "PM" };
    let clock = format!("{hour}:{} {suffix}", format_args!("{:02}", date.minute()));
    if date.date() == now.date() {
        Ok(clock)
    } else {
        Ok(format!(
            "{clock} · {}/{}/{}",
            u8::from(date.month()),
            date.day(),
            date.year()
        ))
    }
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn truncate_utf16(value: &str, limit: usize) -> String {
    if utf16_len(value) <= limit {
        return value.to_owned();
    }
    let keep = limit.saturating_sub(1);
    let mut units = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > keep {
            break;
        }
        units = next;
        end = index + character.len_utf8();
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_uses_javascript_utf16_width() {
        assert_eq!(truncate_utf16("abc", 3), "abc");
        assert_eq!(truncate_utf16("a𝄞bc", 4), "a𝄞…");
    }

    #[test]
    fn project_scope_uses_the_current_checkout() {
        let project =
            oc_paths::project::resolve_project(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(!project.id.is_empty());
    }
}

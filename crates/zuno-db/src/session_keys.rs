//! The session-keyed tables that no foreign key reaches, read out of the live schema.
//!
//! Zuno has two paths that remove a session's rows: [`crate::session::remove`], which the
//! HTTP and TUI delete verbs take for one subtree, and [`crate::prune`], which the CLI and
//! the server maintenance handler take for a retention selection. Both have to reach every
//! table that holds a row keyed by a session, and a table whose session key carries no
//! foreign key is reached by neither `ON DELETE CASCADE` nor luck — it has to be named.
//!
//! Naming it twice, by hand, is what leaked: `verification_receipt` was added to one list
//! after review, and `human_request` and `provider_retry_backoff` were still behind it in
//! both. `human_request.payload` and `response` hold a question a user was asked and the
//! answer they gave, so the leak was the user's own text surviving the delete that was
//! supposed to remove it, for every pruned session.
//!
//! So the enumeration is read out of SQLite instead of restated: [`uncascaded`] asks which
//! declared tables carry `session_id` or `aggregate_id` with no foreign key on that column,
//! and both paths sweep exactly that set. A table added to [`crate::schema`] later is
//! enrolled in both by existing, not by being remembered twice.
//!
//! Only tables [`crate::schema`] itself declares are returned, and the SQL is built from the
//! `&'static str` names this crate authored rather than from the strings SQLite handed back.
//! A supported database is Zuno's own — [`crate::migration`] refuses an unmarked or foreign
//! format — but a leftover table from an earlier product, or an operator's own copy of the
//! data, must not become deletable just because it happens to carry a `session_id` column.
//!
//! That allowlist is also the limit of the guarantee, and it is narrower than the database.
//! Another crate can create its own tables in this same pool: `zuno_goal::GoalStore::from_pool`
//! runs its schema batch on the application pool, so `goal`, `goal_pause`, `goal_retry` and
//! their siblings live in the file `session` lives in, keyed by `session_id` and carrying no
//! foreign key to it. This module does not return them, so neither delete path removes them,
//! and `goal.objective` — the user's own words — outlives the session that was deleted.
//! Enrolling them cannot be decided here: widening to every live session-keyed table would
//! make an operator's own copy of a table deletable, and the owning crate has to either
//! declare the foreign key through a guarded migration or register its tables. Until it does,
//! the invariant these two paths hold is "every session-keyed table *this crate declares*",
//! not "every row keyed by this session".

use rusqlite::{Connection, Transaction, params};
use zuno_error::DbError;

use crate::open;
use crate::schema;

/// The columns a session-scoped row is keyed by.
///
/// `aggregate_id` is the event log's spelling of the same key (`event.ts:513-523`).
const KEY_COLUMNS: [&str; 2] = ["aggregate_id", "session_id"];

/// One table and key column that deleting the `session` row cannot reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionKey {
    /// Table name as [`crate::schema`] declares it.
    pub(crate) table: &'static str,
    /// Key column, one of [`KEY_COLUMNS`], as this module spells it.
    pub(crate) column: &'static str,
}

/// Every declared table whose session key no foreign key covers, ordered by name.
///
/// `session` itself is excluded: it is deleted by primary key, not swept. `event` is absent
/// because its `aggregate_id` does carry a foreign key, so the `event_sequence` delete
/// cascades to it — callers that want to be independent of the `foreign_keys` pragma still
/// name it explicitly.
///
/// # Errors
///
/// [`DbError::Query`] if the schema cannot be read.
pub(crate) fn uncascaded(connection: &Connection) -> Result<Vec<SessionKey>, DbError> {
    let declared = schema::declared_tables();
    let mut statement = connection
        .prepare(
            "SELECT tbl.name, col.name
             FROM sqlite_schema AS tbl
             JOIN pragma_table_info(tbl.name) AS col
             WHERE tbl.type = 'table'
               AND tbl.name <> 'session'
               AND col.name IN ('aggregate_id', 'session_id')
               AND NOT EXISTS (
                 SELECT 1 FROM pragma_foreign_key_list(tbl.name) AS fk
                 WHERE fk.\"from\" = col.name
               )
             ORDER BY tbl.name, col.name",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(open::map_error)?;
    let mut keys = Vec::new();
    for row in rows {
        let (table, column) = row.map_err(open::map_error)?;
        // Both halves are resolved back to a name this crate authored, and only the resolved
        // spelling is ever interpolated into SQL. SQLite compares object names
        // case-insensitively for ASCII, so the resolved name addresses the same table.
        let Some(table) = declared
            .iter()
            .copied()
            .find(|declared| declared.eq_ignore_ascii_case(&table))
        else {
            continue;
        };
        let Some(column) = KEY_COLUMNS
            .iter()
            .copied()
            .find(|known| known.eq_ignore_ascii_case(&column))
        else {
            continue;
        };
        keys.push(SessionKey { table, column });
    }
    Ok(keys)
}

/// Delete every row these keys hold for one session id.
///
/// # Errors
///
/// [`DbError::Query`] if any statement fails.
pub(crate) fn sweep_one(
    transaction: &Transaction<'_>,
    keys: &[SessionKey],
    session_id: &str,
) -> Result<(), DbError> {
    for key in keys {
        transaction
            .execute(
                &format!("DELETE FROM {} WHERE {} = ?1", key.table, key.column),
                params![session_id],
            )
            .map_err(open::map_error)?;
    }
    Ok(())
}

/// Delete every row these keys hold for a JSON array of ids.
///
/// The caller supplies the id set, because the two paths do not have the same one: a subtree
/// delete knows its session ids, while [`crate::prune`] additionally expands an event
/// aggregate id set. `aggregate_json` is used for `aggregate_id` columns and `session_json`
/// for the rest, so a table added later on either key is covered by the set that fits it.
///
/// # Errors
///
/// [`DbError::Query`] if any statement fails.
pub(crate) fn sweep_many(
    transaction: &Transaction<'_>,
    keys: &[SessionKey],
    session_json: &str,
    aggregate_json: &str,
) -> Result<(), DbError> {
    for key in keys {
        let binding = if key.column == "aggregate_id" {
            aggregate_json
        } else {
            session_json
        };
        transaction
            .execute(
                &format!(
                    "DELETE FROM {} WHERE {} IN (SELECT value FROM json_each(?1))",
                    key.table, key.column
                ),
                params![binding],
            )
            .map_err(open::map_error)?;
    }
    Ok(())
}

/// Column names of one declared table, in schema order, for row-size accounting.
///
/// Names that are not plain identifiers are dropped rather than quoted: the current schema
/// uses none, and a name this crate did not author has no business being interpolated into
/// SQL. Dropping one understates that column's bytes; it can never widen what is deleted,
/// because deletion is keyed by the session id and not by this list.
///
/// # Errors
///
/// [`DbError::Query`] if the column list cannot be read.
pub(crate) fn columns(connection: &Connection, table: &str) -> Result<Vec<String>, DbError> {
    let mut statement = connection
        .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([table], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?;
    let mut names = Vec::new();
    for row in rows {
        let name = row.map_err(open::map_error)?;
        if is_plain_identifier(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

/// Whether this name is an unquoted-safe SQLite identifier.
fn is_plain_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_is_not_a_plain_identifier_is_refused() {
        for name in ["", "1abc", "a-b", "a b", "a\"b", "tbl;DROP", "naïve"] {
            assert!(
                !is_plain_identifier(name),
                "{name:?} must never be interpolated into SQL"
            );
        }
        for name in ["a", "_a", "session_id", "tokens_cache_write", "a1_B2"] {
            assert!(is_plain_identifier(name), "{name:?} is the current schema");
        }
    }
}

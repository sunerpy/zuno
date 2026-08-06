//! Opt-in FTS5 indexes for lexical recall across persisted conversations.
//!
//! The compatibility schema must remain byte-for-byte equal to the TypeScript
//! binary, so [`ensure`] owns these objects instead of `schema::up`. Text lives
//! in `part.data`, while ranking and navigation are message-shaped; the source
//! views therefore aggregate parts onto the stable identity of `message.rowid`.
//! This adapts Hermes' external-content and trigram design
//! (`hermes_state_common.py:403-521`) to opencode's message/part split.
//!
//! SQLite may renumber implicit rowids during `VACUUM`. Call [`rebuild`] after a
//! vacuum so the external-content indexes are rebound to the new message rowids.

use oc_error::DbError;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::open::map_error;

const DDL: &str = r#"
CREATE VIEW IF NOT EXISTS message_fts_source AS
SELECT
  m.rowid AS rowid,
  m.id AS message_id,
  m.session_id AS session_id,
  json_extract(m.data, '$.role') AS role,
  m.time_created AS time_created,
  COALESCE(group_concat(
    CASE json_extract(p.data, '$.type')
      WHEN 'text' THEN json_extract(p.data, '$.text')
      WHEN 'reasoning' THEN json_extract(p.data, '$.text')
      WHEN 'subtask' THEN trim(
        COALESCE(json_extract(p.data, '$.description'), '') || ' ' ||
        COALESCE(json_extract(p.data, '$.prompt'), '') || ' ' ||
        COALESCE(json_extract(p.data, '$.command'), '')
      )
      WHEN 'tool' THEN trim(
        COALESCE(json_extract(p.data, '$.tool'), '') || ' ' ||
        COALESCE(json_extract(p.data, '$.state.title'), '') || ' ' ||
        COALESCE(json_extract(p.data, '$.state.input'), '') || ' ' ||
        COALESCE(json_extract(p.data, '$.state.output'), '')
      )
      ELSE NULL
    END,
    char(10)
  ), '') AS content
FROM message AS m
LEFT JOIN part AS p ON p.message_id = m.id
GROUP BY m.rowid;

CREATE VIEW IF NOT EXISTS message_fts_trigram_source AS
SELECT
  m.rowid AS rowid,
  COALESCE(group_concat(
    CASE json_extract(p.data, '$.type')
      WHEN 'text' THEN json_extract(p.data, '$.text')
      WHEN 'reasoning' THEN json_extract(p.data, '$.text')
      WHEN 'subtask' THEN trim(
        COALESCE(json_extract(p.data, '$.description'), '') || ' ' ||
        COALESCE(json_extract(p.data, '$.prompt'), '') || ' ' ||
        COALESCE(json_extract(p.data, '$.command'), '')
      )
      ELSE NULL
    END,
    char(10)
  ), '') AS content
FROM message AS m
LEFT JOIN part AS p
  ON p.message_id = m.id
 AND json_extract(p.data, '$.type') <> 'tool'
GROUP BY m.rowid;

CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
  content,
  content='message_fts_source',
  content_rowid='rowid'
);

CREATE VIRTUAL TABLE IF NOT EXISTS message_fts_trigram USING fts5(
  content,
  content='message_fts_trigram_source',
  content_rowid='rowid',
  tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS message_fts_message_insert
AFTER INSERT ON message BEGIN
  INSERT INTO message_fts(rowid, content)
  SELECT rowid, content FROM message_fts_source WHERE rowid = new.rowid;
  INSERT INTO message_fts_trigram(rowid, content)
  SELECT rowid, content FROM message_fts_trigram_source WHERE rowid = new.rowid;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_message_delete
BEFORE DELETE ON message BEGIN
  INSERT INTO message_fts(message_fts, rowid, content)
  SELECT 'delete', rowid, content FROM message_fts_source WHERE rowid = old.rowid;
  INSERT INTO message_fts_trigram(message_fts_trigram, rowid, content)
  SELECT 'delete', rowid, content FROM message_fts_trigram_source WHERE rowid = old.rowid;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_part_insert_before
BEFORE INSERT ON part BEGIN
  INSERT INTO message_fts(message_fts, rowid, content)
  SELECT 'delete', rowid, content FROM message_fts_source WHERE message_id = new.message_id;
  INSERT INTO message_fts_trigram(message_fts_trigram, rowid, content)
  SELECT 'delete', t.rowid, t.content
  FROM message_fts_trigram_source AS t
  JOIN message AS m ON m.rowid = t.rowid
  WHERE m.id = new.message_id;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_part_insert_after
AFTER INSERT ON part BEGIN
  INSERT INTO message_fts(rowid, content)
  SELECT rowid, content FROM message_fts_source WHERE message_id = new.message_id;
  INSERT INTO message_fts_trigram(rowid, content)
  SELECT t.rowid, t.content
  FROM message_fts_trigram_source AS t
  JOIN message AS m ON m.rowid = t.rowid
  WHERE m.id = new.message_id;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_part_delete_before
BEFORE DELETE ON part BEGIN
  INSERT INTO message_fts(message_fts, rowid, content)
  SELECT 'delete', rowid, content FROM message_fts_source WHERE message_id = old.message_id;
  INSERT INTO message_fts_trigram(message_fts_trigram, rowid, content)
  SELECT 'delete', t.rowid, t.content
  FROM message_fts_trigram_source AS t
  JOIN message AS m ON m.rowid = t.rowid
  WHERE m.id = old.message_id;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_part_delete_after
AFTER DELETE ON part BEGIN
  INSERT INTO message_fts(rowid, content)
  SELECT rowid, content FROM message_fts_source WHERE message_id = old.message_id;
  INSERT INTO message_fts_trigram(rowid, content)
  SELECT t.rowid, t.content
  FROM message_fts_trigram_source AS t
  JOIN message AS m ON m.rowid = t.rowid
  WHERE m.id = old.message_id;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_part_update_before
BEFORE UPDATE OF message_id, data ON part BEGIN
  INSERT INTO message_fts(message_fts, rowid, content)
  SELECT 'delete', rowid, content FROM message_fts_source WHERE message_id = old.message_id;
  INSERT INTO message_fts_trigram(message_fts_trigram, rowid, content)
  SELECT 'delete', t.rowid, t.content
  FROM message_fts_trigram_source AS t
  JOIN message AS m ON m.rowid = t.rowid
  WHERE m.id = old.message_id;
END;

CREATE TRIGGER IF NOT EXISTS message_fts_part_update_after
AFTER UPDATE OF message_id, data ON part BEGIN
  INSERT INTO message_fts(rowid, content)
  SELECT rowid, content FROM message_fts_source
  WHERE message_id = old.message_id AND old.message_id <> new.message_id;
  INSERT INTO message_fts_trigram(rowid, content)
  SELECT t.rowid, t.content
  FROM message_fts_trigram_source AS t
  JOIN message AS m ON m.rowid = t.rowid
  WHERE m.id = old.message_id AND old.message_id <> new.message_id;
  INSERT INTO message_fts(rowid, content)
  SELECT rowid, content FROM message_fts_source WHERE message_id = new.message_id;
  INSERT INTO message_fts_trigram(rowid, content)
  SELECT t.rowid, t.content
  FROM message_fts_trigram_source AS t
  JOIN message AS m ON m.rowid = t.rowid
  WHERE m.id = new.message_id;
END;
"#;

/// Which FTS tokenizer produced a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFlavor {
    /// SQLite's default `unicode61` tokenizer for lexical terms and operators.
    Unicode,
    /// Overlapping character trigrams for CJK and other substring-oriented text.
    Trigram,
}

/// One ranked message match, before session-level deduplication.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Message identity used as the scroll anchor.
    pub message_id: String,
    /// Owning session identity.
    pub session_id: String,
    /// Validated role copied from `message.data` by the source view.
    pub role: String,
    /// Message creation time in Unix milliseconds.
    pub time_created: i64,
    /// FTS5 BM25 score; lower values are more relevant.
    pub rank: f64,
    /// A bounded highlighted excerpt generated by FTS5.
    pub snippet: String,
    /// Tokenizer selected for the query.
    pub flavor: SearchFlavor,
}

/// Install and backfill the optional FTS objects in one immediate transaction.
///
/// Keeping this explicit preserves the 20-table compatibility contract of
/// `migration::apply`. Repeated calls are safe and do not rebuild an existing
/// index; use [`rebuild`] after an operation such as `VACUUM` that can renumber
/// implicit message rowids.
///
/// # Errors
///
/// Returns [`DbError::Query`] or [`DbError::Busy`] when SQLite cannot create,
/// populate, or commit the objects.
pub fn ensure(connection: &mut Connection) -> Result<(), DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    transaction.execute_batch(DDL).map_err(map_error)?;
    let unicode_documents: i64 = transaction
        .query_row("SELECT count(*) FROM message_fts_docsize", [], |row| {
            row.get(0)
        })
        .map_err(map_error)?;
    let trigram_documents: i64 = transaction
        .query_row(
            "SELECT count(*) FROM message_fts_trigram_docsize",
            [],
            |row| row.get(0),
        )
        .map_err(map_error)?;
    let messages: i64 = transaction
        .query_row("SELECT count(*) FROM message", [], |row| row.get(0))
        .map_err(map_error)?;
    if unicode_documents != messages || trigram_documents != messages {
        rebuild_indexes(&transaction)?;
    }
    transaction.commit().map_err(map_error)
}

/// Rebuild both external-content indexes from their current source views.
///
/// This is required after `VACUUM`, which may renumber the implicit rowids used
/// as FTS document ids. Normal inserts, updates, deletes, and cascades are kept
/// synchronized by the triggers installed by [`ensure`].
///
/// # Errors
///
/// Returns [`DbError::Query`] when either FTS5 rebuild command fails.
pub fn rebuild(connection: &Connection) -> Result<(), DbError> {
    rebuild_indexes(connection)
}

fn rebuild_indexes(connection: &Connection) -> Result<(), DbError> {
    connection
        .execute(
            "INSERT INTO message_fts(message_fts) VALUES ('rebuild')",
            [],
        )
        .map_err(map_error)?;
    connection
        .execute(
            "INSERT INTO message_fts_trigram(message_fts_trigram) VALUES ('rebuild')",
            [],
        )
        .map_err(map_error)?;
    Ok(())
}

/// Search with the tokenizer inferred from the query's script.
///
/// Queries containing CJK characters use the trigram table; all others retain
/// FTS5's normal boolean, phrase, and prefix syntax.
///
/// # Errors
///
/// Returns [`DbError::Query`] for invalid FTS5 syntax or a SQLite read failure.
pub fn search(connection: &Connection, query: &str, limit: u32) -> Result<Vec<SearchHit>, DbError> {
    let flavor = if contains_cjk(query) {
        SearchFlavor::Trigram
    } else {
        SearchFlavor::Unicode
    };
    search_with(connection, query, flavor, limit)
}

/// Search one explicitly selected index and return BM25-ranked message hits.
///
/// # Errors
///
/// Returns [`DbError::Query`] for invalid FTS5 syntax or a SQLite read failure.
pub fn search_with(
    connection: &Connection,
    query: &str,
    flavor: SearchFlavor,
    limit: u32,
) -> Result<Vec<SearchHit>, DbError> {
    if query.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let (table, source) = match flavor {
        SearchFlavor::Unicode => ("message_fts", "message_fts_source"),
        SearchFlavor::Trigram => ("message_fts_trigram", "message_fts_source"),
    };
    let sql = format!(
        "SELECT source.message_id, source.session_id, source.role, source.time_created, \
                bm25({table}) AS rank, \
                snippet({table}, 0, '<mark>', '</mark>', ' … ', 24) AS snippet \
         FROM {table} \
         JOIN {source} AS source ON source.rowid = {table}.rowid \
         WHERE {table} MATCH ?1 \
         ORDER BY rank ASC, source.time_created DESC, source.message_id DESC \
         LIMIT ?2"
    );
    let mut statement = connection.prepare(&sql).map_err(map_error)?;
    let rows = statement
        .query_map(params![query.trim(), limit], |row| {
            Ok(SearchHit {
                message_id: row.get(0)?,
                session_id: row.get(1)?,
                role: row.get(2)?,
                time_created: row.get(3)?,
                rank: row.get(4)?,
                snippet: row.get(5)?,
                flavor,
            })
        })
        .map_err(map_error)?;
    let mut hits = Vec::new();
    for row in rows {
        hits.push(row.map_err(map_error)?);
    }
    Ok(hits)
}

/// Return the flattened searchable content for one message.
///
/// # Errors
///
/// Returns [`DbError::Query`] on a SQLite failure.
pub fn message_content(
    connection: &Connection,
    message_id: &str,
) -> Result<Option<String>, DbError> {
    connection
        .query_row(
            "SELECT content FROM message_fts_source WHERE message_id = ?1",
            [message_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_error)
}

fn contains_cjk(query: &str) -> bool {
    query.chars().any(|character| {
        matches!(
            character,
            '\u{3400}'..='\u{4dbf}'
                | '\u{4e00}'..='\u{9fff}'
                | '\u{f900}'..='\u{faff}'
                | '\u{3040}'..='\u{30ff}'
                | '\u{ac00}'..='\u{d7af}'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_detection_does_not_route_ordinary_unicode_to_trigram() {
        assert!(contains_cjk("数据库连接"));
        assert!(contains_cjk("接続エラー"));
        assert!(!contains_cjk("café connection"));
        assert!(!contains_cjk("database handshake"));
    }
}

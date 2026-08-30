use std::io::{BufRead as _, IsTerminal as _, Write as _};

use rusqlite::types::ValueRef;
use serde_json::{Map, Number, Value};

use super::db_maint;
use crate::command::{DbArgs, DbFormat};

pub(super) fn execute(args: &DbArgs) -> Result<(), String> {
    if args.query.as_deref() == Some("path") {
        println!("{}", zuno_paths::db_path().as_oracle_string());
        return Ok(());
    }
    if let Some(command) = args.query.as_deref().and_then(db_maint::Maintenance::parse) {
        return db_maint::execute(command, args.format);
    }

    let pool = zuno_db::Pool::open_default().map_err(|error| error.to_string())?;
    let mut connection = pool.get().map_err(|error| error.to_string())?;
    zuno_db::migration::apply(&mut connection).map_err(|error| error.to_string())?;

    match &args.query {
        Some(query) => run_query(&connection, query, args.format),
        None => repl(&connection, args.format),
    }
}

fn run_query(
    connection: &rusqlite::Connection,
    query: &str,
    format: DbFormat,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(query)
        .map_err(|error| error.to_string())?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
    let column_count = names.len();
    let mut cursor = statement.query([]).map_err(|error| error.to_string())?;
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().map_err(|error| error.to_string())? {
        let mut values = Vec::with_capacity(column_count);
        for column in 0..column_count {
            values.push(value(
                row.get_ref(column).map_err(|error| error.to_string())?,
            ));
        }
        rows.push(values);
    }
    render(&names, &rows, format)
}

fn value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::Number(Number::from(value)),
        ValueRef::Real(value) => Number::from_f64(value).map_or(Value::Null, Value::Number),
        ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => Value::Array(
            value
                .iter()
                .copied()
                .map(Number::from)
                .map(Value::Number)
                .collect(),
        ),
    }
}

fn render(names: &[String], rows: &[Vec<Value>], format: DbFormat) -> Result<(), String> {
    match format {
        DbFormat::Json => {
            let output: Vec<Value> = rows
                .iter()
                .map(|row| {
                    Value::Object(
                        names
                            .iter()
                            .cloned()
                            .zip(row.iter().cloned())
                            .collect::<Map<_, _>>(),
                    )
                })
                .collect();
            let text = serde_json::to_string_pretty(&output).map_err(|error| error.to_string())?;
            println!("{text}");
        }
        DbFormat::Tsv if !rows.is_empty() => {
            println!("{}", names.join("\t"));
            for row in rows {
                println!(
                    "{}",
                    row.iter().map(tsv_value).collect::<Vec<_>>().join("\t")
                );
            }
        }
        DbFormat::Tsv => {}
    }
    Ok(())
}

fn tsv_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn repl(connection: &rusqlite::Connection, format: DbFormat) -> Result<(), String> {
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut stdout = std::io::stdout();
    if interactive {
        writeln!(
            stdout,
            "SQLite shell for {}",
            zuno_paths::db_path().as_oracle_string()
        )
        .map_err(|error| error.to_string())?;
        writeln!(stdout, "Enter SQL terminated by ';', or .help for help.")
            .map_err(|error| error.to_string())?;
    }

    let mut pending = String::new();
    for line in stdin.lock().lines() {
        if interactive {
            write!(stdout, "sqlite> ").map_err(|error| error.to_string())?;
            stdout.flush().map_err(|error| error.to_string())?;
        }
        let line = line.map_err(|error| error.to_string())?;
        match line.trim() {
            ".exit" | ".quit" => break,
            ".help" => {
                println!(
                    ".exit  Exit this shell\n.help  Show this help\n.schema Show the database schema\n.tables List tables"
                );
                continue;
            }
            ".tables" => {
                run_query(
                    connection,
                    "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
                    DbFormat::Tsv,
                )?;
                continue;
            }
            ".schema" => {
                run_query(
                    connection,
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name",
                    DbFormat::Tsv,
                )?;
                continue;
            }
            _ => {}
        }
        pending.push_str(&line);
        pending.push('\n');
        if line.trim_end().ends_with(';') {
            if let Err(error) = run_query(connection, &pending, format) {
                eprintln!("{error}");
            }
            pending.clear();
        }
    }
    if !pending.trim().is_empty() {
        run_query(connection, &pending, format)?;
    }
    Ok(())
}

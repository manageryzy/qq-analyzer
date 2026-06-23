use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize)]
pub struct SqliteSampleReport {
    pub input: String,
    pub limit: usize,
    pub tables: Vec<SqliteSampleTable>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SqliteSampleTable {
    pub name: String,
    pub sql: String,
    pub rows: Vec<Value>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SenderRowsReport {
    pub input: String,
    pub sender: i64,
    pub limit_per_table: usize,
    pub max_results: usize,
    pub results: Vec<SenderRowMatch>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SenderRowMatch {
    pub table: String,
    pub rowid: Option<i64>,
    pub sender: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SqliteInspectReport {
    pub input: String,
    pub tables: Vec<SqliteInspectTable>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SqliteInspectTable {
    pub name: String,
    pub rows: Option<i64>,
    pub columns: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SqliteExportReport {
    pub input: String,
    pub out_dir: String,
    pub schema: String,
    pub tables_file: String,
    pub tables: Vec<SqliteExportTable>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SqliteExportTable {
    pub name: String,
    pub csv: String,
    pub rows: Option<usize>,
    pub error: Option<String>,
}

pub fn sample_sqlite(path: &Path, limit: usize) -> anyhow::Result<SqliteSampleReport> {
    let con = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = con.prepare(
        "SELECT name, COALESCE(sql, '') FROM sqlite_master \
         WHERE type='table' ORDER BY name",
    )?;
    let table_rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut tables = Vec::new();
    for table in table_rows {
        let (name, sql) = table?;
        let query = format!("SELECT rowid, * FROM {} LIMIT ?", quote_ident(&name));
        match sample_rows(&con, &query, limit) {
            Ok(rows) => tables.push(SqliteSampleTable {
                name,
                sql,
                rows,
                error: None,
            }),
            Err(err) => tables.push(SqliteSampleTable {
                name,
                sql,
                rows: Vec::new(),
                error: Some(err.to_string()),
            }),
        }
    }
    Ok(SqliteSampleReport {
        input: path.display().to_string(),
        limit,
        tables,
    })
}

pub fn inspect_sqlite(path: &Path) -> anyhow::Result<SqliteInspectReport> {
    let con = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let names = user_table_names(&con)?;
    let mut tables = Vec::new();
    for name in names {
        let columns = table_columns(&con, &name).unwrap_or_default();
        let count_sql = format!("SELECT count(*) FROM {}", quote_ident(&name));
        match con.query_row(&count_sql, [], |row| row.get::<_, i64>(0)) {
            Ok(rows) => tables.push(SqliteInspectTable {
                name,
                rows: Some(rows),
                columns,
                error: None,
            }),
            Err(err) => tables.push(SqliteInspectTable {
                name,
                rows: None,
                columns,
                error: Some(err.to_string()),
            }),
        }
    }
    Ok(SqliteInspectReport {
        input: path.display().to_string(),
        tables,
    })
}

pub fn export_sqlite(
    path: &Path,
    out_dir: &Path,
    force: bool,
) -> anyhow::Result<SqliteExportReport> {
    let con = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    fs::create_dir_all(out_dir)?;
    let csv_dir = out_dir.join("csv");
    fs::create_dir_all(&csv_dir)?;

    let schema_path = out_dir.join("schema.sql");
    let tables_path = out_dir.join("tables.txt");
    if !force {
        for path in [&schema_path, &tables_path] {
            if path.exists() {
                anyhow::bail!(
                    "output exists: {}; pass --force to overwrite",
                    path.display()
                );
            }
        }
    }

    let schema = schema_sql(&con)?;
    fs::write(&schema_path, schema)?;
    let table_names = user_table_names(&con)?;
    fs::write(&tables_path, format!("{}\n", table_names.join("\n")))?;

    let mut tables = Vec::new();
    for name in table_names {
        let csv_path = csv_dir.join(format!("{}.csv", safe_filename(&name)));
        if csv_path.exists() && !force {
            tables.push(SqliteExportTable {
                name,
                csv: csv_path.display().to_string(),
                rows: None,
                error: Some("output exists; pass --force to overwrite".to_string()),
            });
            continue;
        }
        match export_table_csv(&con, &name, &csv_path) {
            Ok(rows) => tables.push(SqliteExportTable {
                name,
                csv: csv_path.display().to_string(),
                rows: Some(rows),
                error: None,
            }),
            Err(err) => tables.push(SqliteExportTable {
                name,
                csv: csv_path.display().to_string(),
                rows: None,
                error: Some(err.to_string()),
            }),
        }
    }

    Ok(SqliteExportReport {
        input: path.display().to_string(),
        out_dir: out_dir.display().to_string(),
        schema: schema_path.display().to_string(),
        tables_file: tables_path.display().to_string(),
        tables,
    })
}

pub fn find_sender_rows(
    path: &Path,
    sender: i64,
    limit_per_table: usize,
    max_results: usize,
) -> anyhow::Result<SenderRowsReport> {
    let con = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = con.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND \
         (name GLOB 'group_*' OR name GLOB 'discuss_*' OR name GLOB 'buddy_*') \
         ORDER BY name",
    )?;
    let table_rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut results = Vec::new();
    for table in table_rows {
        if results.len() >= max_results {
            break;
        }
        let table = table?;
        let query = format!(
            "SELECT rowid, SenderUin FROM {} WHERE SenderUin=? LIMIT ?",
            quote_ident(&table)
        );
        let mut stmt = match con.prepare(&query) {
            Ok(stmt) => stmt,
            Err(err) => {
                results.push(SenderRowMatch {
                    table,
                    rowid: None,
                    sender: None,
                    error: Some(err.to_string()),
                });
                continue;
            }
        };
        let rows = match stmt.query_map((sender, limit_per_table as i64), |row| {
            Ok(SenderRowMatch {
                table: table.clone(),
                rowid: Some(row.get(0)?),
                sender: Some(row.get(1)?),
                error: None,
            })
        }) {
            Ok(rows) => rows,
            Err(err) => {
                results.push(SenderRowMatch {
                    table,
                    rowid: None,
                    sender: None,
                    error: Some(err.to_string()),
                });
                continue;
            }
        };
        for row in rows {
            results.push(row?);
            if results.len() >= max_results {
                break;
            }
        }
    }
    Ok(SenderRowsReport {
        input: path.display().to_string(),
        sender,
        limit_per_table,
        max_results,
        results,
    })
}

fn schema_sql(con: &Connection) -> anyhow::Result<String> {
    let mut stmt =
        con.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut lines = Vec::new();
    for row in rows {
        lines.push(row?);
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn user_table_names(con: &Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = con.prepare(
        "SELECT name FROM sqlite_master \
         WHERE type='table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn table_columns(con: &Connection, table: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = con.prepare(&format!("PRAGMA table_info({})", quote_ident(table)))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn export_table_csv(con: &Connection, table: &str, csv_path: &Path) -> anyhow::Result<usize> {
    let tmp = tmp_path(csv_path, "exporting");
    let file = fs::File::create(&tmp)?;
    let mut writer = BufWriter::new(file);
    let query = format!("SELECT * FROM {}", quote_ident(table));
    let mut stmt = con.prepare(&query)?;
    let names = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    write_csv_row(&mut writer, names.iter().map(String::as_str))?;
    let mut rows = stmt.query([])?;
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        let mut fields = Vec::with_capacity(names.len());
        for idx in 0..names.len() {
            fields.push(value_ref_to_csv(row.get_ref(idx)?));
        }
        write_csv_row(&mut writer, fields.iter().map(String::as_str))?;
        count += 1;
    }
    writer.flush()?;
    fs::rename(tmp, csv_path)?;
    Ok(count)
}

fn value_ref_to_csv(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => String::new(),
        ValueRef::Integer(v) => v.to_string(),
        ValueRef::Real(v) => v.to_string(),
        ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
        ValueRef::Blob(v) => display_blob(v),
    }
}

fn write_csv_row<'a>(
    writer: &mut impl Write,
    fields: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<()> {
    let mut first = true;
    for field in fields {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        write_csv_field(writer, field)?;
    }
    writer.write_all(b"\n")?;
    Ok(())
}

fn write_csv_field(writer: &mut impl Write, field: &str) -> anyhow::Result<()> {
    let needs_quote = field.contains([',', '"', '\n', '\r']);
    if !needs_quote {
        writer.write_all(field.as_bytes())?;
        return Ok(());
    }
    writer.write_all(b"\"")?;
    for ch in field.chars() {
        if ch == '"' {
            writer.write_all(b"\"\"")?;
        } else {
            let mut buf = [0u8; 4];
            writer.write_all(ch.encode_utf8(&mut buf).as_bytes())?;
        }
    }
    writer.write_all(b"\"")?;
    Ok(())
}

fn tmp_path(path: &Path, suffix: &str) -> PathBuf {
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    path.with_extension(format!("{extension}.{suffix}"))
}

fn safe_filename(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '@' | '+') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "_".to_string()
    } else {
        out
    }
}

fn sample_rows(con: &Connection, query: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
    let mut stmt = con.prepare(query)?;
    let names = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let rows = stmt.query_map([limit as i64], |row| {
        let mut item = serde_json::Map::new();
        for (idx, name) in names.iter().enumerate() {
            item.insert(name.clone(), value_ref_to_json(row.get_ref(idx)?));
        }
        Ok(Value::Object(item))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn value_ref_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => json!(v),
        ValueRef::Real(v) => json!(v),
        ValueRef::Text(v) => json!(String::from_utf8_lossy(v).into_owned()),
        ValueRef::Blob(v) => json!(display_blob(v)),
    }
}

fn display_blob(value: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(value) {
        if printable_text(text) {
            return text.to_string();
        }
    }
    if value.len() % 2 == 0 {
        let units = value
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        if let Ok(text) = String::from_utf16(&units) {
            if printable_text(&text) {
                return text;
            }
        }
    }
    #[cfg(feature = "encoding_rs")]
    {
        let (text, _, had_errors) = encoding_rs::GB18030.decode(value);
        if !had_errors && printable_text(&text) {
            return text.into_owned();
        }
    }
    hex_prefix(value, 160)
}

fn printable_text(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|ch| ch >= '\u{20}' || matches!(ch, '\r' | '\n' | '\t'))
}

fn hex_prefix(value: &[u8], limit: usize) -> String {
    value
        .iter()
        .take(limit)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_display_prefers_utf16le_when_printable() {
        let data = b"H\0i\0";
        assert_eq!(display_blob(data), "Hi");
    }

    #[test]
    fn blob_display_falls_back_to_hex_prefix() {
        assert_eq!(display_blob(&[0, 1, 2]), "00 01 02");
    }

    #[test]
    fn quote_ident_escapes_quotes() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn csv_field_quotes_commas_quotes_and_newlines() {
        let mut out = Vec::new();
        write_csv_field(&mut out, "a,\"b\"\n").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\"a,\"\"b\"\"\n\"");
    }

    #[test]
    fn safe_filename_replaces_path_separators() {
        assert_eq!(safe_filename("a/b\\c"), "a_b_c");
    }
}

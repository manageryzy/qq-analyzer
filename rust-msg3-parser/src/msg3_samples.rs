use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;
use serde::Serialize;

#[derive(Clone, Debug)]
pub struct ExportSamplesOptions {
    pub db: PathBuf,
    pub out: PathBuf,
    pub rows_per_table: usize,
    pub tables: Vec<String>,
    pub all_message_tables: bool,
    pub max_tables: Option<usize>,
    pub known_rows: Vec<KnownRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownRow {
    pub table: String,
    pub rowid: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExportSamplesSummary {
    pub db: String,
    pub out: String,
    pub tables_seen: usize,
    pub rows_written: usize,
    pub known_rows_requested: usize,
}

pub fn parse_known_row(spec: &str) -> anyhow::Result<KnownRow> {
    let (table, rowid_text) = spec
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("known row must be TABLE:ROWID"))?;
    if table.is_empty() {
        anyhow::bail!("known row table is empty");
    }
    let rowid = rowid_text.parse::<i64>()?;
    if rowid <= 0 {
        anyhow::bail!("known row rowid must be positive");
    }
    Ok(KnownRow {
        table: table.to_string(),
        rowid,
    })
}

pub fn export_samples(options: &ExportSamplesOptions) -> anyhow::Result<ExportSamplesSummary> {
    if !options.all_message_tables && options.tables.is_empty() && options.known_rows.is_empty() {
        anyhow::bail!("pass at least one --table, --known-row, or --all-message-tables");
    }
    let con = Connection::open_with_flags(&options.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut tables = options.tables.clone();
    if options.all_message_tables {
        tables = message_tables(&con)?;
        if let Some(max_tables) = options.max_tables {
            tables.truncate(max_tables);
        }
    }
    if let Some(parent) = options.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::from("table\trowid\ttime\trand\tsender_uin\tmsgcontent_hex\n");
    let mut seen = BTreeSet::<(String, i64)>::new();
    let mut tables_seen = 0usize;
    let mut rows_written = 0usize;

    for table in &tables {
        if options.rows_per_table == 0 || !has_message_columns(&con, table)? {
            continue;
        }
        tables_seen += 1;
        let sql = format!(
            "SELECT rowid, Time, Rand, SenderUin, MsgContent FROM {} ORDER BY rowid LIMIT ?",
            quote_ident(table)
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map([options.rows_per_table as i64], |row| {
            Ok(sample_row(
                table,
                row.get(0)?,
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default(),
            ))
        })?;
        for row in rows {
            let row = row?;
            if seen.insert((row.table.clone(), row.rowid)) {
                out.push_str(&row.to_tsv_line());
                rows_written += 1;
            }
        }
    }

    for known in &options.known_rows {
        if !has_message_columns(&con, &known.table)? {
            continue;
        }
        let sql = format!(
            "SELECT rowid, Time, Rand, SenderUin, MsgContent FROM {} WHERE rowid=?",
            quote_ident(&known.table)
        );
        let row = con.query_row(&sql, [known.rowid], |row| {
            Ok(sample_row(
                &known.table,
                row.get(0)?,
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default(),
            ))
        });
        let Ok(row) = row else {
            continue;
        };
        if seen.insert((row.table.clone(), row.rowid)) {
            out.push_str(&row.to_tsv_line());
            rows_written += 1;
        }
    }

    fs::write(&options.out, out)?;
    Ok(ExportSamplesSummary {
        db: options.db.display().to_string(),
        out: options.out.display().to_string(),
        tables_seen,
        rows_written,
        known_rows_requested: options.known_rows.len(),
    })
}

fn message_tables(con: &Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = con.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND \
         (name LIKE 'buddy_%' OR name LIKE 'group_%' OR name LIKE 'discuss_%') \
         ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn has_message_columns(con: &Connection, table: &str) -> anyhow::Result<bool> {
    let sql = format!("PRAGMA table_info({})", quote_sql_string(table));
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let cols = rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|col| col.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    Ok(["time", "rand", "senderuin", "msgcontent"]
        .iter()
        .all(|col| cols.contains(*col)))
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn sample_row(
    table: &str,
    rowid: i64,
    time: i64,
    rand: i64,
    sender_uin: i64,
    msgcontent: Vec<u8>,
) -> SampleRow {
    SampleRow {
        table: table.to_string(),
        rowid,
        time,
        rand,
        sender_uin,
        msgcontent,
    }
}

struct SampleRow {
    table: String,
    rowid: i64,
    time: i64,
    rand: i64,
    sender_uin: i64,
    msgcontent: Vec<u8>,
}

impl SampleRow {
    fn to_tsv_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            self.table,
            self.rowid,
            self.time,
            self.rand,
            self.sender_uin,
            hex_lower(&self.msgcontent)
        )
    }
}

fn hex_lower(value: &[u8]) -> String {
    value.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_row() {
        let row = parse_known_row("group_1:42").unwrap();
        assert_eq!(row.table, "group_1");
        assert_eq!(row.rowid, 42);
        assert!(parse_known_row("group_1").is_err());
        assert!(parse_known_row("group_1:0").is_err());
    }

    #[test]
    fn formats_hex_lower() {
        assert_eq!(hex_lower(&[0, 10, 255]), "000aff");
    }
}

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::Connection;
use serde::Serialize;

use crate::config;

const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";
const CFB_MAGIC: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const SMALL_DB_LIMIT: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct DbAnalysisOptions {
    pub input: Option<PathBuf>,
    pub out_dir: Option<PathBuf>,
    pub only: Vec<String>,
    pub db_limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DbAnalysisReport {
    pub root: String,
    pub account: String,
    pub out_dir: String,
    pub databases: usize,
    pub files: Vec<DbAnalysisFile>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DbAnalysisFile {
    pub input: String,
    pub output: String,
    pub size: u64,
    pub detected: String,
}

pub fn analyze_databases(
    root: &Path,
    account: &str,
    options: &DbAnalysisOptions,
) -> anyhow::Result<DbAnalysisReport> {
    let out_dir = options
        .out_dir
        .clone()
        .unwrap_or_else(|| config::account_output_root(root, account).join("analysis-md"));
    fs::create_dir_all(&out_dir)?;
    let mut dbs = collect_inputs(root, account, options)?;
    dbs.sort();
    dbs.dedup();
    if let Some(limit) = options.db_limit {
        dbs.truncate(limit);
    }

    let mut files = Vec::new();
    let index_path = out_dir.join("INDEX.md");
    let mut index_rows = vec![vec![
        "DB".to_string(),
        "Analysis".to_string(),
        "Size".to_string(),
        "Detected".to_string(),
    ]];
    let root_hint = config::account_output_root(root, account);
    for path in dbs {
        let md = out_dir.join(safe_name(&path, &root_hint));
        let detected = detected_label(&path)?;
        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        let body = analyze_one(root, &path)?;
        fs::write(&md, body)?;
        index_rows.push(vec![
            rel(root, &path),
            rel(root, &md),
            size.to_string(),
            detected.clone(),
        ]);
        files.push(DbAnalysisFile {
            input: path.display().to_string(),
            output: md.display().to_string(),
            size,
            detected,
        });
    }

    let index = format!(
        "# Database Analysis Index\n\n- Generated: `{}`\n- Databases analyzed: `{}`\n\n{}\n",
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        files.len(),
        markdown_table(&index_rows)
    );
    fs::write(index_path, index)?;

    Ok(DbAnalysisReport {
        root: root.display().to_string(),
        account: account.to_string(),
        out_dir: out_dir.display().to_string(),
        databases: files.len(),
        files,
    })
}

fn collect_inputs(
    root: &Path,
    account: &str,
    options: &DbAnalysisOptions,
) -> anyhow::Result<Vec<PathBuf>> {
    let bases = if let Some(input) = &options.input {
        vec![input.clone()]
    } else if !options.only.is_empty() {
        options
            .only
            .iter()
            .map(|selector| source_from_selector(root, account, selector))
            .collect::<anyhow::Result<Vec<_>>>()?
    } else {
        default_input_roots(root, account)
    };
    let mut out = Vec::new();
    for base in bases {
        if base.is_file() {
            out.push(base);
        } else if base.is_dir() {
            collect_db_files(&base, &mut out)?;
        }
    }
    if !options.only.is_empty() && options.input.is_some() {
        let filters = options
            .only
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>();
        out.retain(|path| {
            let text = path
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase();
            filters.iter().any(|filter| text.contains(filter))
        });
    }
    Ok(out)
}

fn default_input_roots(root: &Path, account: &str) -> Vec<PathBuf> {
    let output = config::account_output_root(root, account);
    vec![
        output.join("pcqq_live").join("db"),
        output.join("pcqq").join("raw"),
        output.join("prepared").join("db"),
        output.join("prepared").join("pcqq").join("db"),
        output.join("prepared").join("pcqq").join("raw"),
    ]
}

fn source_from_selector(root: &Path, account: &str, selector: &str) -> anyhow::Result<PathBuf> {
    let raw = PathBuf::from(selector);
    if raw.is_absolute() {
        return Ok(raw);
    }
    let output = config::account_output_root(root, account);
    let candidates = [
        output.join("pcqq_live").join("db").join(selector),
        output.join("pcqq").join("raw").join(selector),
        output.join("prepared").join("db").join(selector),
        output
            .join("prepared")
            .join("pcqq")
            .join("db")
            .join(selector),
        output
            .join("prepared")
            .join("pcqq")
            .join("raw")
            .join(selector),
        output.join(selector),
        root.join(account).join(selector),
        root.join(selector),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Ok(output.join(selector))
}

fn collect_db_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            collect_db_files(&path, out)?;
        } else if ty.is_file()
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("db"))
                .unwrap_or(false)
        {
            out.push(path);
        }
    }
    Ok(())
}

fn analyze_one(root: &Path, path: &Path) -> anyhow::Result<String> {
    let head = read_head(path, 128)?;
    if head.starts_with(SQLITE_MAGIC) {
        analyze_sqlite(root, path)
    } else if head.starts_with(&CFB_MAGIC) {
        Ok(analyze_cfb_like(root, path, &head))
    } else {
        Ok(analyze_unknown(root, path, &head))
    }
}

fn analyze_sqlite(root: &Path, path: &Path) -> anyhow::Result<String> {
    let size = path.metadata()?.len();
    let count_all = size <= SMALL_DB_LIMIT;
    let con = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let page_size: i64 = con.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let page_count: i64 = con.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let freelist_count: i64 = con.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    let user_version: i64 = con.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let encoding: String = con.query_row("PRAGMA encoding", [], |row| row.get(0))?;
    let tables = sqlite_tables(&con)?;

    let mut lines = summary_lines(root, path, size, "SQLite format 3");
    lines.extend([
        "## SQLite Metadata".to_string(),
        String::new(),
        format!("- Page size: `{page_size}`"),
        format!("- Page count: `{page_count}`"),
        format!("- Freelist count: `{freelist_count}`"),
        format!("- Encoding: `{encoding}`"),
        format!("- User version: `{user_version}`"),
        format!("- Tables/views: `{}`", tables.len()),
        format!(
            "- Row counts: `{}`",
            if count_all {
                "exact for every table"
            } else {
                "skipped for large database; schema only"
            }
        ),
        String::new(),
    ]);

    let mut overview = vec![vec![
        "Name".to_string(),
        "Type".to_string(),
        "Rows".to_string(),
        "Columns".to_string(),
        "Likely role".to_string(),
    ]];
    let mut details = Vec::new();
    for table in tables {
        let rows = if count_all && table.typ == "table" {
            count_rows(&con, &table.name)
        } else {
            "not counted".to_string()
        };
        overview.push(vec![
            table.name.clone(),
            table.typ.clone(),
            rows.clone(),
            table.columns.len().to_string(),
            likely_role(&table.name),
        ]);
        details.extend(table_detail(&table, &rows));
    }
    lines.extend([
        "## Table Overview".to_string(),
        String::new(),
        markdown_table(&overview),
        String::new(),
        "## Table Details".to_string(),
        String::new(),
    ]);
    lines.extend(details);
    Ok(finish_markdown(lines))
}

#[derive(Clone, Debug)]
struct SqliteTable {
    name: String,
    typ: String,
    sql: String,
    columns: Vec<SqliteColumn>,
    indexes: Vec<SqliteIndex>,
}

#[derive(Clone, Debug)]
struct SqliteColumn {
    cid: i64,
    name: String,
    typ: String,
    notnull: i64,
    pk: i64,
}

#[derive(Clone, Debug)]
struct SqliteIndex {
    seq: i64,
    name: String,
    unique: i64,
    origin: String,
    partial: i64,
}

fn sqlite_tables(con: &Connection) -> anyhow::Result<Vec<SqliteTable>> {
    let mut stmt = con.prepare(
        "SELECT name, type, COALESCE(sql, '') FROM sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
         ORDER BY type, name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut tables = Vec::new();
    for row in rows {
        let (name, typ, sql) = row?;
        let columns = sqlite_columns(con, &name)?;
        let indexes = if typ == "table" {
            sqlite_indexes(con, &name)?
        } else {
            Vec::new()
        };
        tables.push(SqliteTable {
            name,
            typ,
            sql,
            columns,
            indexes,
        });
    }
    Ok(tables)
}

fn sqlite_columns(con: &Connection, table: &str) -> anyhow::Result<Vec<SqliteColumn>> {
    let sql = format!("PRAGMA table_info({})", quote_sql_string(table));
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(SqliteColumn {
            cid: row.get(0)?,
            name: row.get(1)?,
            typ: row.get(2)?,
            notnull: row.get(3)?,
            pk: row.get(5)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn sqlite_indexes(con: &Connection, table: &str) -> anyhow::Result<Vec<SqliteIndex>> {
    let sql = format!("PRAGMA index_list({})", quote_sql_string(table));
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(SqliteIndex {
            seq: row.get(0)?,
            name: row.get(1)?,
            unique: row.get(2)?,
            origin: row.get(3)?,
            partial: row.get(4)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn count_rows(con: &Connection, table: &str) -> String {
    let sql = format!("SELECT count(*) FROM {}", quote_ident(table));
    con.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count.to_string())
        .unwrap_or_else(|err| format!("error: {err}"))
}

fn table_detail(table: &SqliteTable, rows: &str) -> Vec<String> {
    let mut lines = vec![
        format!("### `{}`", table.name),
        String::new(),
        format!("- Type: `{}`", table.typ),
        format!("- Rows: `{rows}`"),
        format!("- Index count: `{}`", table.indexes.len()),
        String::new(),
    ];
    if !table.sql.is_empty() {
        lines.extend([
            "SQL:".to_string(),
            String::new(),
            "```sql".to_string(),
            table.sql.clone(),
            "```".to_string(),
            String::new(),
        ]);
    }
    let mut cols = vec![vec![
        "cid".to_string(),
        "name".to_string(),
        "type".to_string(),
        "notnull".to_string(),
        "pk".to_string(),
    ]];
    for col in &table.columns {
        cols.push(vec![
            col.cid.to_string(),
            col.name.clone(),
            col.typ.clone(),
            col.notnull.to_string(),
            col.pk.to_string(),
        ]);
    }
    lines.push(markdown_table(&cols));
    lines.push(String::new());
    if !table.indexes.is_empty() {
        let mut indexes = vec![vec![
            "seq".to_string(),
            "name".to_string(),
            "unique".to_string(),
            "origin".to_string(),
            "partial".to_string(),
        ]];
        for idx in &table.indexes {
            indexes.push(vec![
                idx.seq.to_string(),
                idx.name.clone(),
                idx.unique.to_string(),
                idx.origin.clone(),
                idx.partial.to_string(),
            ]);
        }
        lines.extend([
            "Indexes:".to_string(),
            String::new(),
            markdown_table(&indexes),
            String::new(),
        ]);
    }
    lines
}

fn analyze_cfb_like(root: &Path, path: &Path, head: &[u8]) -> String {
    let size = path.metadata().map(|m| m.len()).unwrap_or(0);
    let mut lines = summary_lines(root, path, size, "Classic QQ legacy OLE-like container");
    lines.extend([
        "## Legacy Container".to_string(),
        String::new(),
        "- The file has the CFB/OLE magic bytes, but classic QQ DB files may use a private or partially OLE-compatible layout.".to_string(),
        "- Rust preprocessing can extract QQ-tolerant CFB streams with `qq_analyzer_rs preprocess --extract-cfb`.".to_string(),
        String::new(),
    ]);
    if let Some(fields) = cfb_header_fields(head) {
        lines.extend([
            "### Header Fields".to_string(),
            String::new(),
            markdown_table(&fields),
            String::new(),
        ]);
    }
    finish_markdown(lines)
}

fn analyze_unknown(root: &Path, path: &Path, head: &[u8]) -> String {
    let size = path.metadata().map(|m| m.len()).unwrap_or(0);
    let mut lines = summary_lines(root, path, size, "unknown");
    let printable = head
        .iter()
        .map(|b| {
            if (0x20..=0x7e).contains(b) {
                *b as char
            } else {
                '.'
            }
        })
        .collect::<String>();
    lines.extend([
        "## Header".to_string(),
        String::new(),
        format!("- Hex: `{}`", hex_spaced(head)),
        format!("- ASCII: `{printable}`"),
        String::new(),
    ]);
    finish_markdown(lines)
}

fn summary_lines(root: &Path, path: &Path, size: u64, detected: &str) -> Vec<String> {
    vec![
        format!(
            "# {}",
            path.file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("database")
        ),
        String::new(),
        "## Summary".to_string(),
        String::new(),
        format!("- Source: `{}`", rel(root, path)),
        format!("- Size: `{size}` bytes"),
        format!("- Detected type: `{detected}`"),
        format!("- Likely role: {}", likely_role(&path.to_string_lossy())),
        format!(
            "- Generated: `{}`",
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        String::new(),
    ]
}

fn cfb_header_fields(head: &[u8]) -> Option<Vec<Vec<String>>> {
    if head.len() < 80 {
        return None;
    }
    let u16le = |off: usize| u16::from_le_bytes([head[off], head[off + 1]]);
    let u32le =
        |off: usize| u32::from_le_bytes([head[off], head[off + 1], head[off + 2], head[off + 3]]);
    let sector_shift = u16le(30);
    let mini_sector_shift = u16le(32);
    Some(vec![
        vec!["Field".to_string(), "Value".to_string()],
        vec!["minor_version".to_string(), u16le(24).to_string()],
        vec!["major_version".to_string(), u16le(26).to_string()],
        vec!["byte_order".to_string(), format!("0x{:04x}", u16le(28))],
        vec![
            "sector_size".to_string(),
            if sector_shift < 31 {
                (1u64 << sector_shift).to_string()
            } else {
                "0".to_string()
            },
        ],
        vec![
            "mini_sector_size".to_string(),
            if mini_sector_shift < 31 {
                (1u64 << mini_sector_shift).to_string()
            } else {
                "0".to_string()
            },
        ],
        vec!["num_directory_sectors".to_string(), u32le(40).to_string()],
        vec!["num_fat_sectors".to_string(), u32le(44).to_string()],
        vec!["first_directory_sector".to_string(), u32le(48).to_string()],
        vec!["mini_stream_cutoff".to_string(), u32le(56).to_string()],
        vec!["first_mini_fat_sector".to_string(), u32le(60).to_string()],
        vec!["num_mini_fat_sectors".to_string(), u32le(64).to_string()],
        vec!["first_difat_sector".to_string(), u32le(68).to_string()],
        vec!["num_difat_sectors".to_string(), u32le(72).to_string()],
    ])
}

fn detected_label(path: &Path) -> anyhow::Result<String> {
    let head = read_head(path, 16)?;
    if head.starts_with(SQLITE_MAGIC) {
        Ok("SQLite format 3".to_string())
    } else if head.starts_with(&CFB_MAGIC) {
        Ok("Classic QQ legacy OLE-like container".to_string())
    } else {
        Ok("unknown".to_string())
    }
}

fn read_head(path: &Path, len: usize) -> anyhow::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut buf = vec![0u8; len];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn safe_name(path: &Path, root_hint: &Path) -> String {
    let rel = path.strip_prefix(root_hint).unwrap_or(path);
    let text = rel.to_string_lossy().replace(['\\', '/'], "__");
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '@' | '+' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out.push_str(".md");
    out
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn markdown_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(cell.len());
        }
    }
    let mut lines = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let cells = (0..cols)
            .map(|idx| {
                let cell = row.get(idx).cloned().unwrap_or_default();
                format!("{cell:<width$}", width = widths[idx])
            })
            .collect::<Vec<_>>();
        lines.push(format!("| {} |", cells.join(" | ")));
        if row_idx == 0 {
            let sep = widths
                .iter()
                .map(|width| "-".repeat((*width).max(1)))
                .collect::<Vec<_>>();
            lines.push(format!("| {} |", sep.join(" | ")));
        }
    }
    lines.join("\n")
}

fn likely_role(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let rules = [
        ("msg", "message/chat related"),
        ("index", "search/index related"),
        ("fts", "full-text search related"),
        ("friend", "friend/contact related"),
        ("group", "group related"),
        ("file", "file transfer or file metadata related"),
        ("face", "face/avatar/emoji media related"),
        ("emoji", "emoji/sticker related"),
        ("registry", "configuration/registry related"),
        ("pub", "public account related"),
        ("collection", "favorites/collection related"),
        ("audio", "voice/audio related"),
        ("cache", "cache/reporting related"),
    ];
    let mut hits = Vec::new();
    for (needle, label) in rules {
        if lower.contains(needle) && !hits.contains(&label) {
            hits.push(label);
        }
    }
    if hits.is_empty() {
        "unknown from name alone".to_string()
    } else {
        hits.join(", ")
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn hex_spaced(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn finish_markdown(mut lines: Vec<String>) -> String {
    while lines.last().map(|line| line.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.push(String::new());
    lines.join("\n")
}

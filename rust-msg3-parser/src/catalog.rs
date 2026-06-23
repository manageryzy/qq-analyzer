use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::config;
use crate::preprocess::PreprocessReport;

#[derive(Clone, Debug, Serialize)]
pub struct CatalogSummary {
    pub path: String,
    pub metadata_rows: i64,
    pub preprocess_steps: i64,
    pub prepared_sources: i64,
    pub credential_records: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PreparedSource {
    pub role: String,
    pub ok: bool,
    pub source_path: String,
    pub prepared_path: String,
    pub effective_path: String,
    pub source_exists: bool,
    pub prepared_exists: bool,
    pub detail: String,
}

pub fn default_catalog_path(root: &Path, account: &str) -> PathBuf {
    config::account_output_root(root, account)
        .join("catalog")
        .join("catalog.sqlite")
}

pub fn source_by_role(path: &Path, role: &str) -> anyhow::Result<Option<PreparedSource>> {
    if !path.is_file() {
        return Ok(None);
    }
    let con = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let row = con
        .query_row(
            "select role, ok, source_path, prepared_path, detail
             from prepared_sources
             where role = ?1
             order by rowid desc
             limit 1",
            params![role],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((role, ok, source_path, prepared_path, detail)) = row else {
        return Ok(None);
    };
    let source = portable_path(&source_path);
    let prepared = portable_path(&prepared_path);
    let prepared_exists = prepared.exists();
    let source_exists = source.exists();
    let effective = if prepared_exists {
        prepared.display().to_string()
    } else if source_exists {
        source.display().to_string()
    } else {
        String::new()
    };
    Ok(Some(PreparedSource {
        role,
        ok: ok != 0,
        source_path,
        prepared_path,
        effective_path: effective,
        source_exists,
        prepared_exists,
        detail,
    }))
}

fn portable_path(path: &str) -> PathBuf {
    let raw = PathBuf::from(path);
    if raw.exists() {
        return raw;
    }
    #[cfg(not(windows))]
    {
        let bytes = path.as_bytes();
        if bytes.len() >= 3
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
            && bytes[0].is_ascii_alphabetic()
        {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            let rest = path[3..].replace('\\', "/");
            return PathBuf::from(format!("/mnt/{drive}/{rest}"));
        }
    }
    raw
}

pub fn write_preprocess_report(path: &Path, report: &PreprocessReport) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut con = Connection::open(path)?;
    init_schema(&con)?;
    let tx = con.transaction()?;
    tx.execute("delete from metadata", [])?;
    tx.execute("delete from preprocess_steps", [])?;
    tx.execute("delete from prepared_sources", [])?;
    tx.execute("delete from credential_summary", [])?;

    tx.execute(
        "insert into metadata(key, value) values (?1, ?2), (?3, ?4), (?5, ?6)",
        params![
            "root",
            report.root,
            "account",
            report.account,
            "prepared_root",
            report.prepared_root
        ],
    )?;
    tx.execute(
        "insert into credential_summary(path, records, pcqq_sqlite_keys, infostorage_tea_keys, ntqq_sqlcipher_keys)
         values (?1, ?2, ?3, ?4, ?5)",
        params![
            report.credentials.path,
            report.credentials.records as i64,
            report.credentials.pcqq_sqlite_keys as i64,
            report.credentials.infostorage_tea_keys as i64,
            report.credentials.ntqq_sqlcipher_keys as i64,
        ],
    )?;
    for (idx, step) in report.steps.iter().enumerate() {
        tx.execute(
            "insert into preprocess_steps(seq, name, ok, input, output, detail)
             values (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                idx as i64,
                step.name,
                if step.ok { 1 } else { 0 },
                step.input,
                step.output,
                step.detail,
            ],
        )?;
        tx.execute(
            "insert into prepared_sources(role, ok, source_path, prepared_path, detail)
             values (?1, ?2, ?3, ?4, ?5)",
            params![
                step.name,
                if step.ok { 1 } else { 0 },
                step.input,
                step.output,
                step.detail,
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn summarize(path: &Path) -> anyhow::Result<CatalogSummary> {
    let con = Connection::open(path)?;
    Ok(CatalogSummary {
        path: path.display().to_string(),
        metadata_rows: count(&con, "metadata")?,
        preprocess_steps: count(&con, "preprocess_steps")?,
        prepared_sources: count(&con, "prepared_sources")?,
        credential_records: con
            .query_row(
                "select coalesce(sum(records), 0) from credential_summary",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0),
    })
}

fn init_schema(con: &Connection) -> anyhow::Result<()> {
    con.execute_batch(
        r#"
        create table if not exists metadata(
            key text primary key,
            value text not null
        );
        create table if not exists credential_summary(
            path text not null,
            records integer not null,
            pcqq_sqlite_keys integer not null,
            infostorage_tea_keys integer not null,
            ntqq_sqlcipher_keys integer not null
        );
        create table if not exists preprocess_steps(
            seq integer not null,
            name text not null,
            ok integer not null,
            input text not null,
            output text not null,
            detail text not null
        );
        create table if not exists prepared_sources(
            role text not null,
            ok integer not null,
            source_path text not null,
            prepared_path text not null,
            detail text not null
        );
        create index if not exists idx_prepared_sources_role on prepared_sources(role);
        create index if not exists idx_preprocess_steps_name on preprocess_steps(name);
        "#,
    )?;
    Ok(())
}

fn count(con: &Connection, table: &str) -> anyhow::Result<i64> {
    let sql = format!("select count(*) from {table}");
    Ok(con.query_row(&sql, [], |row| row.get(0))?)
}

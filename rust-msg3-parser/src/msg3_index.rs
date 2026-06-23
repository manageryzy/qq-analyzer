use std::path::PathBuf;

use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Default)]
pub struct IndexQueryOptions {
    pub db: PathBuf,
    pub conversation_account: Option<i64>,
    pub like: Option<String>,
    pub fts_match: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct IndexQueryReport {
    pub db: String,
    pub limit: usize,
    pub queries: Vec<IndexQueryResult>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IndexQueryResult {
    pub name: String,
    pub sql: String,
    pub rows: Vec<Value>,
    pub error: Option<String>,
}

pub fn query_index(options: &IndexQueryOptions) -> anyhow::Result<IndexQueryReport> {
    let con = Connection::open_with_flags(&options.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut queries = Vec::new();
    let limit = options.limit.max(1);

    if let Some(account) = options.conversation_account {
        queries.push(run_query(
            &con,
            "account",
            "SELECT rowid,msgcontent,chattype,account,senduin,time,rowidofmsg,seq \
             FROM msgindex WHERE account=? ORDER BY time,rowid LIMIT ?",
            &[Param::Integer(account), Param::Integer(limit as i64)],
        ));
        queries.push(run_query(
            &con,
            "content_account",
            "SELECT rowid,c0msgcontent AS msgcontent,c2chattype AS chattype,c3account AS account,\
             c4senduin AS senduin,c5time AS time,c7rowidofmsg AS rowidofmsg,c8seq AS seq \
             FROM msgindex_content WHERE c3account=? ORDER BY c5time,rowid LIMIT ?",
            &[Param::Integer(account), Param::Integer(limit as i64)],
        ));
    }

    if let Some(pattern) = &options.like {
        queries.push(run_query(
            &con,
            "content_like",
            "SELECT rowid,c0msgcontent AS msgcontent,c2chattype AS chattype,c3account AS account,\
             c4senduin AS senduin,c5time AS time,c7rowidofmsg AS rowidofmsg,c8seq AS seq \
             FROM msgindex_content WHERE c0msgcontent LIKE ? LIMIT ?",
            &[Param::Text(pattern.clone()), Param::Integer(limit as i64)],
        ));
    }

    if let Some(pattern) = &options.fts_match {
        queries.push(run_query(
            &con,
            "match",
            "SELECT rowid,msgcontent,chattype,account,senduin,time,rowidofmsg,seq \
             FROM msgindex WHERE msgindex MATCH ? LIMIT ?",
            &[Param::Text(pattern.clone()), Param::Integer(limit as i64)],
        ));
    }

    if queries.is_empty() {
        anyhow::bail!("pass at least one of --conversation-account, --like, or --match");
    }

    Ok(IndexQueryReport {
        db: options.db.display().to_string(),
        limit,
        queries,
    })
}

fn run_query(con: &Connection, name: &str, sql: &str, params: &[Param]) -> IndexQueryResult {
    match run_query_inner(con, sql, params) {
        Ok(rows) => IndexQueryResult {
            name: name.to_string(),
            sql: sql.to_string(),
            rows,
            error: None,
        },
        Err(err) => IndexQueryResult {
            name: name.to_string(),
            sql: sql.to_string(),
            rows: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

fn run_query_inner(con: &Connection, sql: &str, params: &[Param]) -> anyhow::Result<Vec<Value>> {
    let mut stmt = con.prepare(sql)?;
    let names = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        let mut item = serde_json::Map::new();
        for (idx, name) in names.iter().enumerate() {
            item.insert(name.clone(), value_ref_to_json(row.get_ref(idx)?));
        }
        Ok(Value::Object(item))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

#[derive(Clone, Debug)]
enum Param {
    Integer(i64),
    Text(String),
}

impl rusqlite::ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            Param::Integer(value) => value.to_sql(),
            Param::Text(value) => value.to_sql(),
        }
    }
}

fn value_ref_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => json!(v),
        ValueRef::Real(v) => json!(v),
        ValueRef::Text(v) => json!(String::from_utf8_lossy(v).into_owned()),
        ValueRef::Blob(v) => json!(hex_lower(v)),
    }
}

fn hex_lower(value: &[u8]) -> String {
    value.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_values_are_hex() {
        assert_eq!(
            value_ref_to_json(ValueRef::Blob(&[0, 10, 255])),
            json!("000aff")
        );
    }
}

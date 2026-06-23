use std::env;
use std::path::PathBuf;

use anyhow::Context;
use rusqlite::Connection;
use serde_json::json;

use msg3_richtext_parser_rs::config;
use msg3_richtext_parser_rs::msg3_parser as parser;

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn main() -> anyhow::Result<()> {
    let mut root = config::default_root()?;
    let mut account = None::<String>;
    let mut table = None::<String>;
    let mut rowid = None::<i64>;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().context("--root needs value")?),
            "--account" => account = Some(args.next().context("--account needs value")?),
            "--table" => table = Some(args.next().context("--table needs value")?),
            "--rowid" => rowid = Some(args.next().context("--rowid needs value")?.parse()?),
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }

    let table = table.ok_or_else(|| {
        anyhow::anyhow!("--table <msg3-table> is required, for example group_<uin>")
    })?;
    let rowid = rowid.ok_or_else(|| anyhow::anyhow!("--rowid <rowid> is required"))?;
    let account = config::resolve_account(&root, account)?;
    let db = config::msg3_db(&root, &account);
    let con = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| db.display().to_string())?;
    let sql = format!(
        "select MsgContent from {} where rowid=?",
        quote_ident(&table)
    );
    let data: Vec<u8> = con.query_row(&sql, [rowid], |row| row.get(0))?;
    let (text, elements_json, rich_nodes_json, rich_html) = parser::parse_msgcontent_outputs(&data);
    let out = json!({
        "table": table,
        "rowid": rowid,
        "msgcontent_len": data.len(),
        "text": text,
        "rich_nodes": serde_json::from_str::<serde_json::Value>(&rich_nodes_json)?,
        "elements": serde_json::from_str::<serde_json::Value>(&elements_json)?,
        "rich_html": rich_html,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

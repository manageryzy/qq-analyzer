use std::env;
use std::path::PathBuf;

use anyhow::Context;
use rusqlite::Connection;

use msg3_richtext_parser_rs::config;

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn hex_window(data: &[u8], start: usize, len: usize) -> String {
    let end = data.len().min(start.saturating_add(len));
    let mut out = String::new();
    let mut pos = start;
    while pos < end {
        out.push_str(&format!("{pos:06x}: "));
        for b in &data[pos..end.min(pos + 16)] {
            out.push_str(&format!("{b:02x} "));
        }
        out.push_str(" |");
        for b in &data[pos..end.min(pos + 16)] {
            let ch = if (0x20..=0x7e).contains(b) {
                *b as char
            } else {
                '.'
            };
            out.push(ch);
        }
        out.push_str("|\n");
        pos += 16;
    }
    out
}

fn main() -> anyhow::Result<()> {
    let mut root = config::default_root()?;
    let mut account = None::<String>;
    let mut table = None::<String>;
    let mut rowid = None::<i64>;
    let mut start = 0usize;
    let mut len = 512usize;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().context("--root needs value")?),
            "--account" => account = Some(args.next().context("--account needs value")?),
            "--table" => table = Some(args.next().context("--table needs value")?),
            "--rowid" => rowid = Some(args.next().context("--rowid needs value")?.parse()?),
            "--start" => start = args.next().context("--start needs value")?.parse()?,
            "--len" => len = args.next().context("--len needs value")?.parse()?,
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
    println!("table={table} rowid={rowid} msgcontent_len={}", data.len());
    print!("{}", hex_window(&data, start, len));
    Ok(())
}

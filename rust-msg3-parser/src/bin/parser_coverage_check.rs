use std::cmp::max;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use msg3_richtext_parser_rs::config;
use msg3_richtext_parser_rs::msg3_parser as parser;

#[derive(Serialize)]
struct Gap {
    start: usize,
    end: usize,
    len: usize,
    hex: String,
}

#[derive(Serialize)]
struct Sample {
    table: String,
    rowid: i64,
    msgcontent_len: usize,
    gaps: Vec<Gap>,
    info_issues: Vec<String>,
}

#[derive(Default, Serialize)]
struct Counters {
    tables: usize,
    rows_checked: usize,
    rows_with_gaps: usize,
    total_gaps: usize,
    total_gap_bytes: usize,
    known_wrapper_gaps: usize,
    json_errors: usize,
    parser_panics: usize,
    info_records_checked: usize,
    info_json_errors: usize,
    info_parser_panics: usize,
    info_partial_txdata_records: usize,
    info_revoke_gray_tip_rows: usize,
    info_replace_rows: usize,
    unclassified_elements: usize,
}

#[derive(Serialize)]
struct Report {
    db: String,
    table_filter: String,
    limit_per_table: usize,
    counters: Counters,
    issue_counts: BTreeMap<String, usize>,
    samples: Vec<Sample>,
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn inc(map: &mut BTreeMap<String, usize>, key: &str) {
    *map.entry(key.to_string()).or_default() += 1;
}

fn table_names(con: &Connection, filter: &str) -> anyhow::Result<Vec<String>> {
    if !filter.is_empty() {
        return Ok(vec![filter.to_string()]);
    }
    let mut stmt = con.prepare(
        "select name from sqlite_master where type='table' \
         and (name like 'group_%' or name like 'buddy_%' or name like 'discuss_%') \
         order by name",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn table_has_column(con: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    let mut stmt = con.prepare(&format!("pragma table_info({})", quote_ident(table)))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row?.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn hex_bytes(data: &[u8], max_len: usize) -> String {
    let mut out = String::new();
    for (i, b) in data.iter().take(max_len).enumerate() {
        if i != 0 {
            out.push(' ');
        }
        out.push_str(&format!("{b:02x}"));
    }
    if data.len() > max_len {
        out.push_str(" ...");
    }
    out
}

fn ignorable_gap(data: &[u8]) -> bool {
    data.iter().all(|b| matches!(*b, 0x00 | 0x20))
}

fn known_msg3_wrapper_gap(start: usize, data: &[u8]) -> bool {
    if start != 20 || data.is_empty() || data.len() > 128 {
        return false;
    }
    let Some((&typ, rest)) = data.split_first() else {
        return false;
    };
    let plausible_type = matches!(
        typ,
        1 | 2
            | 3
            | 5
            | 6
            | 7
            | 0x0c
            | 0x0d
            | 0x0e
            | 0x10
            | 0x11
            | 0x12
            | 0x14
            | 0x15
            | 0x16
            | 0x18
            | 0x19
            | 0x1a
            | 0x1b
            | 0x1e
    );
    if plausible_type && rest.len() >= 2 {
        let n = u16::from_le_bytes([rest[0], rest[1]]) as usize;
        if n <= data.len().saturating_sub(3) {
            return false;
        }
    }
    true
}

fn element_range(elem: &Value, msg_len: usize) -> Option<(usize, usize)> {
    let offset = elem.get("offset")?.as_u64()? as usize;
    let payload_len = elem.get("payload_len")?.as_u64()? as usize;
    let typ = elem.get("type").and_then(Value::as_u64).unwrap_or(255);
    let start = offset;
    let end = if typ == 0 {
        offset.checked_add(payload_len)?
    } else {
        offset.checked_add(3)?.checked_add(payload_len)?
    };
    if start <= msg_len && end <= msg_len && start < end {
        Some((start, end))
    } else {
        None
    }
}

fn coverage_gaps(msg: &[u8], elements: &Value, counters: &mut Counters) -> Vec<Gap> {
    let mut ranges = Vec::new();
    if msg.len() >= 20 && msg.starts_with(b"MSG") {
        ranges.push((0usize, 20usize));
    }
    for elem in elements.as_array().into_iter().flatten() {
        let status = elem
            .get("display_status")
            .and_then(Value::as_str)
            .unwrap_or("unclassified");
        if !matches!(status, "rendered" | "ignored") {
            counters.unclassified_elements += 1;
            continue;
        }
        if let Some(range) = element_range(elem, msg.len()) {
            ranges.push(range);
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = max(last.1, end);
                continue;
            }
        }
        merged.push((start, end));
    }
    let mut gaps = Vec::new();
    let mut pos = 0usize;
    for (start, end) in merged {
        if pos < start {
            let bytes = &msg[pos..start];
            if known_msg3_wrapper_gap(pos, bytes) {
                counters.known_wrapper_gaps += 1;
            } else if !ignorable_gap(bytes) {
                gaps.push(Gap {
                    start: pos,
                    end: start,
                    len: start - pos,
                    hex: hex_bytes(bytes, 48),
                });
            }
        }
        pos = max(pos, end);
    }
    if pos < msg.len() {
        let bytes = &msg[pos..];
        if known_msg3_wrapper_gap(pos, bytes) {
            counters.known_wrapper_gaps += 1;
        } else if !ignorable_gap(bytes) {
            gaps.push(Gap {
                start: pos,
                end: msg.len(),
                len: msg.len() - pos,
                hex: hex_bytes(bytes, 48),
            });
        }
    }
    gaps
}

fn scan_info(
    info: &[u8],
    counters: &mut Counters,
    issue_counts: &mut BTreeMap<String, usize>,
) -> Vec<String> {
    let mut issues = Vec::new();
    if info.is_empty() {
        return issues;
    }
    counters.info_records_checked += 1;
    let parsed = std::panic::catch_unwind(|| parser::parse_info_json(info));
    let info_json = match parsed {
        Ok(value) => value,
        Err(_) => {
            counters.info_parser_panics += 1;
            inc(issue_counts, "info_parser_panics");
            issues.push("info_parser_panics".to_string());
            return issues;
        }
    };
    let value: Value = match serde_json::from_str(&info_json) {
        Ok(value) => value,
        Err(_) => {
            counters.info_json_errors += 1;
            inc(issue_counts, "info_json_errors");
            issues.push("info_json_errors".to_string());
            return issues;
        }
    };
    let mut has_revoke = false;
    let mut has_replace = false;
    for rec in value["td_ta_records"].as_array().into_iter().flatten() {
        if rec["txdata_complete"].as_bool() == Some(false) {
            counters.info_partial_txdata_records += 1;
            inc(issue_counts, "info_partial_txdata_records");
            if !issues
                .iter()
                .any(|item| item == "info_partial_txdata_records")
            {
                issues.push("info_partial_txdata_records".to_string());
            }
        }
        for field in rec["txdata_fields"].as_array().into_iter().flatten() {
            let name = field["name"].as_str().unwrap_or("");
            let value_uint = field["value_uint"].as_u64().unwrap_or(0);
            if name == "bMsgRevokeGrayTip" && value_uint != 0 {
                has_revoke = true;
            }
            if name == "bReplace" && value_uint != 0 {
                has_replace = true;
            }
        }
    }
    if has_revoke {
        counters.info_revoke_gray_tip_rows += 1;
    }
    if has_replace {
        counters.info_replace_rows += 1;
    }
    issues
}

fn main() -> anyhow::Result<()> {
    let mut root = config::default_root()?;
    let mut account = None::<String>;
    let mut table_filter = String::new();
    let mut limit_per_table = 256usize;
    let mut out_path: Option<PathBuf> = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().context("--root needs value")?),
            "--account" => account = Some(args.next().context("--account needs value")?),
            "--table" => table_filter = args.next().context("--table needs value")?,
            "--limit" => limit_per_table = args.next().context("--limit needs value")?.parse()?,
            "--out" => out_path = Some(PathBuf::from(args.next().context("--out needs value")?)),
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }

    let account = config::resolve_account(&root, account)?;
    let db = config::msg3_db(&root, &account);
    let con = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| db.display().to_string())?;
    let mut report = Report {
        db: db.display().to_string(),
        table_filter: table_filter.clone(),
        limit_per_table,
        counters: Counters::default(),
        issue_counts: BTreeMap::new(),
        samples: Vec::new(),
    };

    for table in table_names(&con, &table_filter)? {
        if !table_has_column(&con, &table, "MsgContent")? {
            continue;
        }
        report.counters.tables += 1;
        let quoted = quote_ident(&table);
        let has_info = table_has_column(&con, &table, "Info")?;
        let sql = if has_info {
            format!("select rowid, MsgContent, Info from {quoted} order by rowid limit ?")
        } else {
            format!("select rowid, MsgContent, null from {quoted} order by rowid limit ?")
        };
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt.query_map([limit_per_table as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?.unwrap_or_default(),
            ))
        })?;
        for row in rows {
            let (rowid, msg, info) = row?;
            report.counters.rows_checked += 1;
            let info_issues = scan_info(&info, &mut report.counters, &mut report.issue_counts);
            let parsed = std::panic::catch_unwind(|| parser::parse_msgcontent_outputs(&msg));
            let (_, elements_json, _, _) = match parsed {
                Ok(v) => v,
                Err(_) => {
                    report.counters.parser_panics += 1;
                    inc(&mut report.issue_counts, "parser_panics");
                    continue;
                }
            };
            let elements: Value = match serde_json::from_str(&elements_json) {
                Ok(v) => v,
                Err(_) => {
                    report.counters.json_errors += 1;
                    inc(&mut report.issue_counts, "json_errors");
                    continue;
                }
            };
            let gaps = coverage_gaps(&msg, &elements, &mut report.counters);
            if !gaps.is_empty() || !info_issues.is_empty() {
                report.counters.rows_with_gaps += 1;
                report.counters.total_gaps += gaps.len();
                report.counters.total_gap_bytes += gaps.iter().map(|gap| gap.len).sum::<usize>();
                if !gaps.is_empty() {
                    inc(&mut report.issue_counts, "coverage_gap");
                }
                if report.samples.len() < 64 {
                    report.samples.push(Sample {
                        table: table.clone(),
                        rowid,
                        msgcontent_len: msg.len(),
                        gaps,
                        info_issues,
                    });
                }
            }
        }
    }

    let json = serde_json::to_string_pretty(&report)?;
    if let Some(path) = out_path {
        fs::write(path, json)?;
    } else {
        println!("{json}");
    }
    if report.counters.parser_panics > 0
        || report.counters.info_parser_panics > 0
        || report.counters.json_errors > 0
        || report.counters.info_json_errors > 0
        || report.counters.info_partial_txdata_records > 0
        || report.counters.unclassified_elements > 0
        || report.counters.rows_with_gaps > 0
    {
        std::process::exit(2);
    }
    Ok(())
}

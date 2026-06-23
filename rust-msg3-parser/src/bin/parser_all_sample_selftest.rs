use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use msg3_richtext_parser_rs::config;
use msg3_richtext_parser_rs::msg3_parser as parser;

#[derive(Default, Serialize)]
struct Counters {
    tables: usize,
    rows_total_estimate: i64,
    sampled_rows: usize,
    json_errors: usize,
    empty_text_and_nodes: usize,
    empty_text_and_html: usize,
    blank_text_messages: usize,
    partial_txdata_records: usize,
    records_with_trailer: usize,
    parse_errors: usize,
    suspicious_text: usize,
    unrendered_media_candidates: usize,
    rich_empty_href: usize,
    media_node_type_mismatch: usize,
    media_node_missing_candidates: usize,
    info_json_errors: usize,
    info_partial_txdata_records: usize,
    info_revoke_gray_tip_rows: usize,
    info_replace_rows: usize,
}

#[derive(Serialize)]
struct Coverage {
    edge: i64,
    points: i64,
    random_points: i64,
    max_table_sampled_rows: usize,
}

#[derive(Serialize)]
struct TableReport {
    table: String,
    rows: i64,
    max_rowid: i64,
    sampled_rows: usize,
    issue_counts: BTreeMap<String, usize>,
}

#[derive(Serialize)]
struct Sample {
    table: String,
    rowid: i64,
    msgcontent_len: usize,
    text: String,
    rich_nodes: Value,
    element_summary: Value,
    info_summary: Value,
    issues: Vec<String>,
}

#[derive(Serialize)]
struct Report {
    db: String,
    coverage: Coverage,
    counters: Counters,
    issue_counts: BTreeMap<String, usize>,
    tables: Vec<TableReport>,
    samples: Vec<Sample>,
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn inc(map: &mut BTreeMap<String, usize>, key: &str) {
    *map.entry(key.to_string()).or_default() += 1;
}

fn table_names(con: &Connection) -> anyhow::Result<Vec<String>> {
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

fn compact_elements(elements: &Value) -> Value {
    let mut out = Vec::new();
    for elem in elements.as_array().into_iter().flatten().take(8) {
        let mut obj = serde_json::Map::new();
        for key in ["offset", "type", "kind", "payload_len", "summary"] {
            if let Some(value) = elem.get(key) {
                obj.insert(key.to_string(), value.clone());
            }
        }
        let mut record_summaries = Vec::new();
        for rec in elem["td_ta_records"]
            .as_array()
            .into_iter()
            .flatten()
            .take(4)
        {
            let mut r = serde_json::Map::new();
            for key in [
                "offset",
                "tag",
                "txdata_field_count",
                "txdata_complete",
                "trailer_len",
                "parse_errors",
            ] {
                if let Some(value) = rec.get(key) {
                    r.insert(key.to_string(), value.clone());
                }
            }
            if let Some(fields) = rec["txdata_fields"].as_array() {
                let names = fields
                    .iter()
                    .take(16)
                    .filter_map(|f| f["name"].as_str())
                    .collect::<Vec<_>>();
                r.insert("field_names".to_string(), json!(names));
            }
            record_summaries.push(Value::Object(r));
        }
        if !record_summaries.is_empty() {
            obj.insert("td_ta_records".to_string(), Value::Array(record_summaries));
        }
        out.push(Value::Object(obj));
    }
    Value::Array(out)
}

fn scan_record_errors(
    elements: &Value,
    counters: &mut Counters,
    issues: &mut Vec<String>,
    issue_counts: &mut BTreeMap<String, usize>,
) {
    for elem in elements.as_array().into_iter().flatten() {
        for rec in elem["td_ta_records"].as_array().into_iter().flatten() {
            if rec["txdata_complete"].as_bool() == Some(false) {
                counters.partial_txdata_records += 1;
                if !issues.iter().any(|s| s == "partial_txdata") {
                    issues.push("partial_txdata".to_string());
                }
                inc(issue_counts, "partial_txdata");
            }
            if rec["trailer_len"].as_u64().unwrap_or(0) > 0 {
                counters.records_with_trailer += 1;
                if !issues.iter().any(|s| s == "unparsed_trailer") {
                    issues.push("unparsed_trailer".to_string());
                }
                inc(issue_counts, "unparsed_trailer");
            }
            if let Some(errors) = rec["parse_errors"].as_array() {
                if !errors.is_empty() {
                    counters.parse_errors += errors.len();
                    if !issues.iter().any(|s| s == "parse_errors") {
                        issues.push("parse_errors".to_string());
                    }
                    inc(issue_counts, "parse_errors");
                }
            }
        }
    }
}

fn suspicious_text_score(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        return 0;
    }
    let replacement = text.chars().filter(|ch| *ch == '\u{fffd}').count();
    if replacement > 0 {
        return replacement;
    }
    let risky = text
        .chars()
        .filter(|ch| {
            let c = *ch as u32;
            ((0xe000..=0xf8ff).contains(&c) && c != 0xf8ff)
                || (0x1200..=0x137f).contains(&c)
                || (0x2c80..=0x2cff).contains(&c)
                || (c < 0x20 && *ch != '\n' && *ch != '\r' && *ch != '\t')
        })
        .count();
    if risky >= 2 && risky * 100 / chars > 12 {
        risky
    } else {
        0
    }
}

fn rich_node_texts(nodes: &Value, out: &mut Vec<String>) {
    for node in nodes.as_array().into_iter().flatten() {
        if let Some(text) = node.get("text").and_then(Value::as_str) {
            out.push(text.to_string());
        }
        if let Some(xml) = node.get("xml").and_then(Value::as_str) {
            out.push(xml.to_string());
        }
    }
}

fn suspicious_output_score(text: &str, rich_html: &str, rich_nodes: &Value) -> usize {
    let mut score = suspicious_text_score(text) + suspicious_text_score(rich_html);
    let mut node_texts = Vec::new();
    rich_node_texts(rich_nodes, &mut node_texts);
    for item in node_texts {
        score += suspicious_text_score(&item);
    }
    score
}

fn has_visible_node(nodes: &Value) -> bool {
    nodes
        .as_array()
        .map(|arr| {
            arr.iter().any(|node| {
                node.get("text")
                    .and_then(Value::as_str)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                    || node
                        .get("type")
                        .and_then(Value::as_str)
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn is_media_candidate(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty()
        || text.starts_with('{')
        || text.starts_with("com.tencent.")
        || text.contains('“')
        || text.contains('”')
        || text.contains("文件")
        || text.contains("成功")
        || text.contains("取消")
    {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    let media_exts = [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".mp4", ".webm", ".mov", ".amr", ".mp3",
        ".wav", ".ogg", ".m4a", ".pdf", ".zip", ".rar", ".7z",
    ];
    let has_ext = media_exts.iter().any(|ext| lower.ends_with(ext));
    let url_has_ext = text
        .split(['?', '#'])
        .next()
        .map(|path| {
            let path = path.to_ascii_lowercase();
            media_exts.iter().any(|ext| path.ends_with(ext))
        })
        .unwrap_or(false);
    text.starts_with("UserDataImage:")
        || text.starts_with("UserDataCustomFace:")
        || text.starts_with("UserDataRoot:")
        || text.starts_with("UserDataFileRecv:")
        || text.starts_with("OSRoot:")
        || text.starts_with("sysres:")
        || ((text.starts_with("http://") || text.starts_with("https://")) && url_has_ext)
        || ((text.contains('\\') || text.contains('/')) && has_ext)
        || (text
            .chars()
            .all(|ch| ch.is_ascii() && !ch.is_ascii_control())
            && has_ext)
}

fn collect_string_values(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            let text = s.trim();
            if is_media_candidate(text) && !out.iter().any(|item| item == text) {
                out.push(text.to_string());
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_string_values(item, out);
            }
        }
        Value::Object(map) => {
            let hidden_sysface_metadata = map
                .get("kind")
                .and_then(Value::as_str)
                .map(|kind| kind == "sysface")
                .unwrap_or(false)
                && map
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .is_empty()
                && map.get("td_ta_records").is_some();
            if hidden_sysface_metadata {
                return;
            }
            for item in map.values() {
                collect_string_values(item, out);
            }
        }
        _ => {}
    }
}

fn rich_node_haystack(nodes: &Value) -> String {
    let mut strings = Vec::new();
    collect_string_values(nodes, &mut strings);
    for node in nodes.as_array().into_iter().flatten() {
        if let Some(text) = node.get("text").and_then(Value::as_str) {
            strings.push(text.to_string());
        }
        if let Some(href) = node.get("href").and_then(Value::as_str) {
            strings.push(href.to_string());
        }
    }
    strings.join("\n")
}

fn unrendered_media_candidates(elements: &Value, rich_nodes: &Value) -> Vec<String> {
    let mut candidates = Vec::new();
    collect_string_values(elements, &mut candidates);
    let haystack = rich_node_haystack(rich_nodes);
    candidates
        .into_iter()
        .filter(|candidate| !haystack.contains(candidate))
        .take(8)
        .collect()
}

fn candidate_kind(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if lower.ends_with(".mp4")
        || lower.ends_with(".webm")
        || lower.ends_with(".mov")
        || lower.contains("\\video\\")
        || lower.contains("/video/")
    {
        Some("video")
    } else if lower.ends_with(".amr")
        || lower.ends_with(".mp3")
        || lower.ends_with(".wav")
        || lower.ends_with(".ogg")
        || lower.ends_with(".m4a")
        || lower.contains("\\audio\\")
        || lower.contains("/audio/")
    {
        Some("voice")
    } else if text.starts_with("UserDataImage:")
        || text.starts_with("UserDataCustomFace:")
        || text.starts_with("FaceStore:")
        || text.starts_with("sysres:")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp")
    {
        Some("image")
    } else if is_media_candidate(text) {
        Some("file")
    } else {
        None
    }
}

fn expected_media_kind(candidates: &[String]) -> Option<&'static str> {
    let mut has_image = false;
    let mut has_voice = false;
    let mut has_file = false;
    for candidate in candidates {
        match candidate_kind(candidate) {
            Some("video") => return Some("video"),
            Some("voice") => has_voice = true,
            Some("image") => has_image = true,
            Some("file") => has_file = true,
            _ => {}
        }
    }
    if has_voice {
        Some("voice")
    } else if has_image {
        Some("image")
    } else if has_file {
        Some("file")
    } else {
        None
    }
}

fn scan_rich_node_quality(
    rich_nodes: &Value,
    counters: &mut Counters,
    issues: &mut Vec<String>,
    issue_counts: &mut BTreeMap<String, usize>,
    table_issue_counts: &mut BTreeMap<String, usize>,
) {
    for node in rich_nodes.as_array().into_iter().flatten() {
        let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
        if node_type == "rich" {
            let href = node.get("href").and_then(Value::as_str).unwrap_or("");
            let xml = node.get("xml").and_then(Value::as_str).unwrap_or("");
            if href.trim().is_empty() && xml.trim().is_empty() {
                counters.rich_empty_href += 1;
                issues.push("rich_empty_href".to_string());
                inc(issue_counts, "rich_empty_href");
                inc(table_issue_counts, "rich_empty_href");
            }
        }
        if matches!(node_type, "image" | "video" | "voice" | "file") {
            let candidates = node
                .get("candidates")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if candidates.is_empty() {
                counters.media_node_missing_candidates += 1;
                issues.push(format!("media_node_missing_candidates:{node_type}"));
                inc(issue_counts, "media_node_missing_candidates");
                inc(table_issue_counts, "media_node_missing_candidates");
            } else if let Some(expected) = expected_media_kind(&candidates) {
                if expected != node_type {
                    counters.media_node_type_mismatch += 1;
                    issues.push(format!("media_node_type_mismatch:{node_type}->{expected}"));
                    inc(issue_counts, "media_node_type_mismatch");
                    inc(table_issue_counts, "media_node_type_mismatch");
                }
            }
        }
    }
}

fn scan_info(
    info: &[u8],
    counters: &mut Counters,
    issues: &mut Vec<String>,
    issue_counts: &mut BTreeMap<String, usize>,
    table_issue_counts: &mut BTreeMap<String, usize>,
) -> Value {
    if info.is_empty() {
        return json!({});
    }
    let info_json = parser::parse_info_json(info);
    let parsed: Value = match serde_json::from_str(&info_json) {
        Ok(value) => value,
        Err(err) => {
            counters.info_json_errors += 1;
            issues.push(format!("info_json_error:{err}"));
            inc(issue_counts, "info_json_error");
            inc(table_issue_counts, "info_json_error");
            return json!({});
        }
    };
    let mut summary = serde_json::Map::new();
    let mut has_revoke = false;
    let mut has_replace = false;
    for rec in parsed["td_ta_records"].as_array().into_iter().flatten() {
        if rec["txdata_complete"].as_bool() == Some(false) {
            counters.info_partial_txdata_records += 1;
            if !issues.iter().any(|item| item == "info_partial_txdata") {
                issues.push("info_partial_txdata".to_string());
            }
            inc(issue_counts, "info_partial_txdata");
            inc(table_issue_counts, "info_partial_txdata");
        }
        for field in rec["txdata_fields"].as_array().into_iter().flatten() {
            let name = field["name"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            if let Some(value) = field.get("value_uint") {
                summary.insert(name.to_string(), value.clone());
            } else if let Some(text) = field["value_texts"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(Value::as_str)
            {
                summary.insert(name.to_string(), json!(text));
            }
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
    Value::Object(summary)
}

fn table_seed(table: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in table.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn collect_rowids(
    con: &Connection,
    table: &str,
    rows: i64,
    max_rowid: i64,
    edge: i64,
    points: i64,
    random_points: i64,
) -> anyhow::Result<Vec<i64>> {
    let quoted = quote_ident(table);
    let mut ids = BTreeSet::new();
    for order in ["asc", "desc"] {
        let sql = format!("select rowid from {quoted} order by rowid {order} limit ?");
        let mut stmt = con.prepare(&sql)?;
        let mapped = stmt.query_map(params![edge], |row| row.get::<_, i64>(0))?;
        for id in mapped {
            ids.insert(id?);
        }
    }
    if max_rowid > 0 && points > 0 {
        for i in 0..points {
            let target = 1 + ((max_rowid - 1) * i / points.max(1));
            let sql =
                format!("select rowid from {quoted} where rowid >= ? order by rowid asc limit 1");
            if let Some(id) = con
                .query_row(&sql, params![target], |row| row.get::<_, i64>(0))
                .optional()?
            {
                ids.insert(id);
            }
        }
        ids.insert(max_rowid);
    }
    if max_rowid > 0 && random_points > 0 {
        let mut state = table_seed(table) ^ ((max_rowid as u64) << 17);
        for _ in 0..random_points {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let target = 1 + (state % max_rowid as u64) as i64;
            let sql =
                format!("select rowid from {quoted} where rowid >= ? order by rowid asc limit 1");
            if let Some(id) = con
                .query_row(&sql, params![target], |row| row.get::<_, i64>(0))
                .optional()?
            {
                ids.insert(id);
            }
        }
    }
    if rows <= edge * 2 + points {
        let sql = format!("select rowid from {quoted} order by rowid asc limit ?");
        let mut stmt = con.prepare(&sql)?;
        let mapped = stmt.query_map(params![edge * 2 + points], |row| row.get::<_, i64>(0))?;
        for id in mapped {
            ids.insert(id?);
        }
    }
    Ok(ids.into_iter().collect())
}

fn main() -> anyhow::Result<()> {
    let mut root = config::default_root()?;
    let mut account = None::<String>;
    let mut edge = 6i64;
    let mut points = 12i64;
    let mut random_points = 0i64;
    let mut sample_limit = 300usize;
    let mut out_path = None::<PathBuf>;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().context("--root needs value")?),
            "--account" => account = Some(args.next().context("--account needs value")?),
            "--edge" => edge = args.next().context("--edge needs value")?.parse()?,
            "--points" => points = args.next().context("--points needs value")?.parse()?,
            "--random-points" => {
                random_points = args
                    .next()
                    .context("--random-points needs value")?
                    .parse()?
            }
            "--sample-limit" => {
                sample_limit = args.next().context("--sample-limit needs value")?.parse()?
            }
            "--out" => out_path = Some(PathBuf::from(args.next().context("--out needs value")?)),
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }

    let account = config::resolve_account(&root, account)?;
    let db = config::msg3_db(&root, &account);
    let con = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| db.display().to_string())?;

    let mut counters = Counters::default();
    let mut issue_counts = BTreeMap::new();
    let mut samples = Vec::new();
    let mut table_reports = Vec::new();

    for table in table_names(&con)? {
        if !table_has_column(&con, &table, "MsgContent")? {
            continue;
        }
        let quoted = quote_ident(&table);
        let (rows, max_rowid): (i64, i64) = con.query_row(
            &format!("select count(*), coalesce(max(rowid), 0) from {quoted}"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        counters.tables += 1;
        counters.rows_total_estimate += rows;
        let rowids = collect_rowids(&con, &table, rows, max_rowid, edge, points, random_points)?;
        let mut table_issue_counts = BTreeMap::new();
        let mut table_sampled = 0usize;

        for rowid in rowids {
            let (msg, info): (Vec<u8>, Vec<u8>) = con.query_row(
                &format!("select MsgContent, Info from {quoted} where rowid=?"),
                params![rowid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            table_sampled += 1;
            counters.sampled_rows += 1;
            let (text, elements_json, rich_nodes_json, rich_html) =
                parser::parse_msgcontent_outputs(&msg);
            let mut issues = Vec::new();
            let elements: Value = match serde_json::from_str(&elements_json) {
                Ok(value) => value,
                Err(err) => {
                    counters.json_errors += 1;
                    issues.push(format!("elements_json_error:{err}"));
                    inc(&mut issue_counts, "elements_json_error");
                    inc(&mut table_issue_counts, "elements_json_error");
                    Value::Array(Vec::new())
                }
            };
            let rich_nodes: Value = match serde_json::from_str(&rich_nodes_json) {
                Ok(value) => value,
                Err(err) => {
                    counters.json_errors += 1;
                    issues.push(format!("rich_nodes_json_error:{err}"));
                    inc(&mut issue_counts, "rich_nodes_json_error");
                    inc(&mut table_issue_counts, "rich_nodes_json_error");
                    Value::Array(Vec::new())
                }
            };

            let issue_count_before = issue_counts.clone();
            scan_record_errors(&elements, &mut counters, &mut issues, &mut issue_counts);
            for (k, after) in &issue_counts {
                let before = issue_count_before.get(k).copied().unwrap_or(0);
                if *after > before {
                    *table_issue_counts.entry(k.clone()).or_default() += after - before;
                }
            }
            let info_summary = scan_info(
                &info,
                &mut counters,
                &mut issues,
                &mut issue_counts,
                &mut table_issue_counts,
            );

            if msg.len() > 20 && text.trim().is_empty() && !has_visible_node(&rich_nodes) {
                counters.empty_text_and_nodes += 1;
                issues.push("empty_text_and_nodes".to_string());
                inc(&mut issue_counts, "empty_text_and_nodes");
                inc(&mut table_issue_counts, "empty_text_and_nodes");
            }
            if msg.len() > 20 && !text.is_empty() && text.trim().is_empty() {
                counters.blank_text_messages += 1;
            }
            if msg.len() > 20 && text.is_empty() && rich_html.is_empty() {
                counters.empty_text_and_html += 1;
                issues.push("empty_text_and_html".to_string());
                inc(&mut issue_counts, "empty_text_and_html");
                inc(&mut table_issue_counts, "empty_text_and_html");
            }
            if suspicious_output_score(&text, &rich_html, &rich_nodes) >= 2 {
                counters.suspicious_text += 1;
                issues.push("suspicious_text".to_string());
                inc(&mut issue_counts, "suspicious_text");
                inc(&mut table_issue_counts, "suspicious_text");
            }
            let leaked = unrendered_media_candidates(&elements, &rich_nodes);
            if !leaked.is_empty() {
                counters.unrendered_media_candidates += leaked.len();
                issues.push(format!("unrendered_media_candidates:{}", leaked.join("|")));
                inc(&mut issue_counts, "unrendered_media_candidates");
                inc(&mut table_issue_counts, "unrendered_media_candidates");
            }
            scan_rich_node_quality(
                &rich_nodes,
                &mut counters,
                &mut issues,
                &mut issue_counts,
                &mut table_issue_counts,
            );

            if !issues.is_empty() && samples.len() < sample_limit {
                samples.push(Sample {
                    table: table.clone(),
                    rowid,
                    msgcontent_len: msg.len(),
                    text: text.chars().take(200).collect(),
                    rich_nodes,
                    element_summary: compact_elements(&elements),
                    info_summary,
                    issues,
                });
            }
        }

        table_reports.push(TableReport {
            table,
            rows,
            max_rowid,
            sampled_rows: table_sampled,
            issue_counts: table_issue_counts,
        });
    }

    let max_table_sampled_rows = table_reports
        .iter()
        .map(|t| t.sampled_rows)
        .max()
        .unwrap_or(0);
    let report = Report {
        db: db.display().to_string(),
        coverage: Coverage {
            edge,
            points,
            random_points,
            max_table_sampled_rows,
        },
        counters,
        issue_counts,
        tables: table_reports,
        samples,
    };
    let text = serde_json::to_string_pretty(&report)?;
    if let Some(path) = out_path {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
    } else {
        println!("{text}");
    }
    Ok(())
}

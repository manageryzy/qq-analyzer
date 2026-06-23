use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

use msg3_richtext_parser_rs::config;
use msg3_richtext_parser_rs::msg3_parser as parser;

#[derive(Default, Serialize)]
struct Counters {
    rows: usize,
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
    media_nodes: usize,
    video_nodes: usize,
    voice_nodes: usize,
    image_nodes: usize,
    file_nodes: usize,
    rows_with_media_nodes: usize,
    info_json_errors: usize,
    info_partial_txdata_records: usize,
    info_revoke_gray_tip_rows: usize,
    info_replace_rows: usize,
}

#[derive(Serialize)]
struct Sample {
    rowid: i64,
    msgcontent_len: usize,
    text: String,
    rich_nodes: Value,
    element_summary: Value,
    info_summary: Value,
    issues: Vec<String>,
    unrendered_media_candidates: Vec<String>,
}

#[derive(Serialize)]
struct Report {
    table: String,
    db: String,
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

fn scan_nodes(nodes: &Value, counters: &mut Counters) {
    let mut row_has_media = false;
    for node in nodes.as_array().into_iter().flatten() {
        match node["type"].as_str().unwrap_or("") {
            "image" => {
                counters.image_nodes += 1;
                counters.media_nodes += 1;
                row_has_media = true;
            }
            "video" => {
                counters.video_nodes += 1;
                counters.media_nodes += 1;
                row_has_media = true;
            }
            "voice" => {
                counters.voice_nodes += 1;
                counters.media_nodes += 1;
                row_has_media = true;
            }
            "file" => {
                counters.file_nodes += 1;
                counters.media_nodes += 1;
                row_has_media = true;
            }
            _ => {}
        }
    }
    if row_has_media {
        counters.rows_with_media_nodes += 1;
    }
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
            } else if let Some(expected) = expected_media_kind(&candidates) {
                if expected != node_type {
                    counters.media_node_type_mismatch += 1;
                    issues.push(format!("media_node_type_mismatch:{node_type}->{expected}"));
                    inc(issue_counts, "media_node_type_mismatch");
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

fn main() -> anyhow::Result<()> {
    let mut root = config::default_root()?;
    let mut account = None::<String>;
    let mut table = None::<String>;
    let mut sample_limit = 200usize;
    let mut out_path = None::<PathBuf>;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().context("--root needs value")?),
            "--account" => account = Some(args.next().context("--account needs value")?),
            "--table" => table = Some(args.next().context("--table needs value")?),
            "--sample-limit" => {
                sample_limit = args.next().context("--sample-limit needs value")?.parse()?
            }
            "--out" => out_path = Some(PathBuf::from(args.next().context("--out needs value")?)),
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }

    let table = table.ok_or_else(|| {
        anyhow::anyhow!("--table <msg3-group-table> is required, for example group_<uin>")
    })?;
    let account = config::resolve_account(&root, account)?;
    let db = config::msg3_db(&root, &account);
    let con = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| db.display().to_string())?;
    let sql = format!(
        "select rowid, MsgContent, Info from {} order by rowid",
        quote_ident(&table)
    );
    let mut stmt = con.prepare(&sql)?;
    let mut rows = stmt.query([])?;

    let mut counters = Counters::default();
    let mut issue_counts = BTreeMap::new();
    let mut samples = Vec::new();

    while let Some(row) = rows.next()? {
        counters.rows += 1;
        let rowid: i64 = row.get(0)?;
        let msg: Vec<u8> = row.get(1)?;
        let info: Vec<u8> = row.get(2)?;
        let (text, elements_json, rich_nodes_json, rich_html) =
            parser::parse_msgcontent_outputs(&msg);
        let mut issues = Vec::new();
        let elements: Value = match serde_json::from_str(&elements_json) {
            Ok(value) => value,
            Err(err) => {
                counters.json_errors += 1;
                issues.push(format!("elements_json_error:{err}"));
                inc(&mut issue_counts, "elements_json_error");
                Value::Array(Vec::new())
            }
        };
        let rich_nodes: Value = match serde_json::from_str(&rich_nodes_json) {
            Ok(value) => value,
            Err(err) => {
                counters.json_errors += 1;
                issues.push(format!("rich_nodes_json_error:{err}"));
                inc(&mut issue_counts, "rich_nodes_json_error");
                Value::Array(Vec::new())
            }
        };

        scan_record_errors(&elements, &mut counters, &mut issues, &mut issue_counts);
        let info_summary = scan_info(&info, &mut counters, &mut issues, &mut issue_counts);
        scan_nodes(&rich_nodes, &mut counters);
        scan_rich_node_quality(&rich_nodes, &mut counters, &mut issues, &mut issue_counts);

        let leaked = unrendered_media_candidates(&elements, &rich_nodes);
        if !leaked.is_empty() {
            counters.unrendered_media_candidates += leaked.len();
            issues.push(format!("unrendered_media_candidates:{}", leaked.join("|")));
            inc(&mut issue_counts, "unrendered_media_candidates");
        }

        if msg.len() > 20 && text.trim().is_empty() && !has_visible_node(&rich_nodes) {
            counters.empty_text_and_nodes += 1;
            issues.push("empty_text_and_nodes".to_string());
            inc(&mut issue_counts, "empty_text_and_nodes");
        }
        if msg.len() > 20 && !text.is_empty() && text.trim().is_empty() {
            counters.blank_text_messages += 1;
        }
        if msg.len() > 20 && text.is_empty() && rich_html.is_empty() {
            counters.empty_text_and_html += 1;
            issues.push("empty_text_and_html".to_string());
            inc(&mut issue_counts, "empty_text_and_html");
        }
        if suspicious_output_score(&text, &rich_html, &rich_nodes) >= 2 {
            counters.suspicious_text += 1;
            issues.push("suspicious_text".to_string());
            inc(&mut issue_counts, "suspicious_text");
        }

        if !issues.is_empty() && samples.len() < sample_limit {
            samples.push(Sample {
                rowid,
                msgcontent_len: msg.len(),
                text: text.chars().take(200).collect(),
                rich_nodes,
                element_summary: compact_elements(&elements),
                info_summary,
                issues,
                unrendered_media_candidates: leaked,
            });
        }
    }

    let report = Report {
        table,
        db: db.display().to_string(),
        counters,
        issue_counts,
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

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::msg3_log_service_time::iso_time;

pub(crate) fn attach_mmp_items_to_multi_msg(nodes: &mut Value, mmp_items: &Value, account: &str) {
    let Some(items) = mmp_items.as_array() else {
        return;
    };
    if items.is_empty() {
        return;
    }
    let mut display_items = items.clone();
    for item in &mut display_items {
        annotate_mmp_item_display(item, account);
    }
    attach_mmp_items_inner(nodes, &display_items);
}

fn annotate_mmp_item_display(item: &mut Value, account: &str) {
    let Some(obj) = item.as_object_mut() else {
        return;
    };
    if let Some(time) = obj.get("time").and_then(Value::as_i64) {
        obj.insert("datetime".to_string(), json!(iso_time(time)));
    }
    let sender_name = obj
        .get("sender_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let sender_uin = obj
        .get("sender_uin")
        .and_then(|value| {
            value
                .as_u64()
                .map(|uin| uin.to_string())
                .or_else(|| value.as_str().map(|uin| uin.to_string()))
        })
        .unwrap_or_default();
    let display_sender = if !sender_name.is_empty() {
        sender_name
    } else {
        sender_uin.clone()
    };
    if !display_sender.is_empty() {
        obj.insert("display_sender".to_string(), json!(display_sender.clone()));
        obj.insert(
            "display_sender_line".to_string(),
            json!(
                if !sender_uin.is_empty() && sender_uin != display_sender && sender_uin != account {
                    format!("{display_sender}({sender_uin})")
                } else {
                    display_sender
                }
            ),
        );
    }
}

fn attach_mmp_items_inner(node: &mut Value, mmp_items: &[Value]) -> bool {
    if let Some(nodes) = node.as_array_mut() {
        for item in nodes {
            if attach_mmp_items_inner(item, mmp_items) {
                return true;
            }
        }
        return false;
    }
    if !node.is_object() {
        return false;
    }
    if node["type"].as_str() == Some("multi_msg") {
        if let Some(obj) = node.as_object_mut() {
            obj.insert("expand_status".to_string(), json!("expanded_from_info_mmp"));
            obj.insert(
                "expand_reason".to_string(),
                json!("Info.buffMsgPackListStream 已解析为内嵌 MsgPackList"),
            );
            obj.insert(
                "items_expanded".to_string(),
                Value::Array(mmp_items.to_vec()),
            );
            obj.insert("expanded_count".to_string(), json!(mmp_items.len()));
        }
        return true;
    }
    for key in ["children", "items", "items_expanded", "nodes", "rich_nodes"] {
        if let Some(child) = node.get_mut(key) {
            if attach_mmp_items_inner(child, mmp_items) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn message_style_meta(elements: &Value) -> Value {
    let mut common_fields = Vec::new();
    let mut text_metadata = Vec::new();
    let mut layout_metadata = Vec::new();
    let mut style_id: Option<u64> = None;
    if let Some(items) = elements.as_array() {
        for (idx, element) in items.iter().enumerate() {
            let typ = element["type"].as_i64().unwrap_or(-1);
            if typ == 25 {
                for pf in element["proto_fields"].as_array().into_iter().flatten() {
                    let path = pf["path"].as_str().unwrap_or("");
                    if !path.starts_with("common_elem.37") {
                        continue;
                    }
                    if path == "common_elem.37.17" {
                        style_id = pf["varint"].as_u64();
                    }
                    common_fields.push(json!({
                        "element_index": idx,
                        "path": path,
                        "field": pf["field"].clone(),
                        "wire": pf["wire"].clone(),
                        "varint": pf["varint"].clone(),
                        "text": pf["text"].clone(),
                        "len": pf["len"].clone(),
                    }));
                }
            } else if typ == 18 {
                for field in element["fields"].as_array().into_iter().flatten() {
                    if let Some(text) = field["text"].as_str() {
                        if !text.trim().is_empty() {
                            text_metadata.push(json!({
                                "element_index": idx,
                                "text": text,
                                "field": field["id"].clone(),
                            }));
                        }
                    }
                }
            } else if typ == 14 {
                for field in element["fields"].as_array().into_iter().flatten() {
                    layout_metadata.push(json!({
                        "element_index": idx,
                        "name": field["name"].clone(),
                        "field": field["id"].clone(),
                        "hex": field["hex"].clone(),
                    }));
                }
            }
        }
    }
    let has_style = !common_fields.is_empty() || !layout_metadata.is_empty();
    json!({
        "has_style": has_style,
        "style_id": style_id,
        "label": style_id.map(|id| format!("样式 {}", id)).unwrap_or_else(|| {
            if has_style { "样式元数据".to_string() } else { String::new() }
        }),
        "common_elem_37": common_fields,
        "text_metadata": text_metadata,
        "layout_metadata": layout_metadata,
    })
}

#[derive(Clone, Default)]
pub(crate) struct QuoteRef {
    seq: Option<u64>,
    sender_uin: Option<u64>,
    time: Option<i64>,
    text: String,
}

pub(crate) fn quote_refs_from_elements(elements: &Value) -> Vec<QuoteRef> {
    let mut out = Vec::new();
    if let Some(items) = elements.as_array() {
        for element in items {
            if element["type"].as_i64() != Some(25) {
                continue;
            }
            let mut quote = QuoteRef {
                text: element["summary"].as_str().unwrap_or("").trim().to_string(),
                ..QuoteRef::default()
            };
            let mut text_best_score = 0usize;
            for pf in element["proto_fields"].as_array().into_iter().flatten() {
                let path = pf["path"].as_str().unwrap_or("");
                if !path.starts_with("common_elem.45") {
                    continue;
                }
                match path {
                    "common_elem.45.1" => quote.seq = pf["varint"].as_u64(),
                    "common_elem.45.2" => quote.sender_uin = pf["varint"].as_u64(),
                    "common_elem.45.3" => quote.time = pf["varint"].as_i64(),
                    _ => {}
                }
                if let Some(text) = pf["text"].as_str() {
                    let trimmed = text.trim_matches(['\n', '\r', '\t', ' ', '\u{0}']).trim();
                    if trimmed.starts_with("u_") {
                        continue;
                    }
                    let score = trimmed.chars().filter(|ch| !ch.is_control()).count()
                        + path.matches('.').count() * 4;
                    if !trimmed.is_empty() && score > text_best_score {
                        quote.text = trimmed.to_string();
                        text_best_score = score;
                    }
                }
            }
            if quote.seq.is_some()
                || quote.sender_uin.is_some()
                || quote.time.is_some()
                || !quote.text.is_empty()
            {
                out.push(quote);
            }
        }
    }
    out
}

pub(crate) fn annotate_quote_nodes(
    con: &Connection,
    quoted_table: &str,
    table: &str,
    current_rowid: i64,
    quote_refs: &[QuoteRef],
    nodes: &mut Value,
) {
    let mut idx = 0usize;
    annotate_quote_nodes_inner(
        con,
        quoted_table,
        table,
        current_rowid,
        quote_refs,
        nodes,
        &mut idx,
    );
}

fn annotate_quote_nodes_inner(
    con: &Connection,
    quoted_table: &str,
    table: &str,
    current_rowid: i64,
    quote_refs: &[QuoteRef],
    node: &mut Value,
    idx: &mut usize,
) {
    if let Some(items) = node.as_array_mut() {
        for item in items {
            annotate_quote_nodes_inner(
                con,
                quoted_table,
                table,
                current_rowid,
                quote_refs,
                item,
                idx,
            );
        }
        return;
    }
    if !node.is_object() {
        return;
    }
    if node["type"].as_str() == Some("quote") {
        if let Some(quote) = quote_refs.get(*idx) {
            let target = resolve_quote_target_rowid(con, quoted_table, current_rowid, quote);
            if let Some(obj) = node.as_object_mut() {
                obj.insert(
                    "quote_ref".to_string(),
                    json!({
                        "seq": quote.seq,
                        "sender_uin": quote.sender_uin.map(|v| v.to_string()),
                        "time": quote.time,
                        "text": quote.text,
                    }),
                );
                if let Some(rowid) = target {
                    obj.insert("target_rowid".to_string(), json!(rowid));
                    obj.insert(
                        "href".to_string(),
                        Value::String(format!("?table={}&offset={}&mode=stream", table, rowid)),
                    );
                }
            }
        }
        *idx += 1;
    }
    for key in ["children", "items", "items_expanded", "nodes", "rich_nodes"] {
        if let Some(child) = node.get_mut(key) {
            annotate_quote_nodes_inner(
                con,
                quoted_table,
                table,
                current_rowid,
                quote_refs,
                child,
                idx,
            );
        }
    }
}

fn resolve_quote_target_rowid(
    con: &Connection,
    quoted_table: &str,
    current_rowid: i64,
    quote: &QuoteRef,
) -> Option<i64> {
    if let (Some(time), Some(sender)) = (quote.time, quote.sender_uin) {
        let sql = format!(
            "select rowid from {quoted_table} where Time = ? and SenderUin = ? and rowid <> ? order by abs(rowid - ?) limit 1"
        );
        if let Ok(rowid) = con.query_row(
            &sql,
            params![time, sender as i64, current_rowid, current_rowid],
            |r| r.get::<_, i64>(0),
        ) {
            return Some(rowid);
        }
    }
    if let Some(time) = quote.time {
        let sql = format!(
            "select rowid from {quoted_table} where Time = ? and rowid <> ? order by abs(rowid - ?) limit 1"
        );
        if let Ok(rowid) = con.query_row(&sql, params![time, current_rowid, current_rowid], |r| {
            r.get::<_, i64>(0)
        }) {
            return Some(rowid);
        }
    }
    None
}

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use serde::Serialize;

use msg3_richtext_parser_rs::txdata_codec::{decode_txdata_fields, TxDataField};

#[derive(Serialize)]
struct FieldOut {
    index: usize,
    header: u8,
    name: String,
    value_len: u32,
    uint_value: Option<u32>,
    texts: Vec<String>,
    hex_prefix: String,
}

#[derive(Serialize)]
struct RecordOut {
    path: String,
    offset: usize,
    len: usize,
    tag: String,
    complete: bool,
    matched: bool,
    fields: Vec<FieldOut>,
}

fn le16(data: &[u8]) -> u16 {
    u16::from_le_bytes([data[0], data[1]])
}

fn uint_value(field: &TxDataField) -> Option<u32> {
    match (field.header, field.value.as_slice()) {
        (1 | 6, [a, b, c, d]) => Some(u32::from_le_bytes([*a, *b, *c, *d])),
        (2, [v]) => Some(*v as u32),
        (3 | 4, [a, b]) => Some(u16::from_le_bytes([*a, *b]) as u32),
        _ => None,
    }
}

fn hex_prefix(data: &[u8], limit: usize) -> String {
    let mut out = data
        .iter()
        .take(limit)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    if data.len() > limit {
        out.push_str(" ...");
    }
    out
}

fn td_record_len(data: &[u8], start: usize) -> Option<usize> {
    if start + 6 > data.len() || !data[start..].starts_with(b"TD\x01\x01") {
        return None;
    }
    let count = le16(&data[start + 4..start + 6]) as usize;
    if count == 0 || count > 200 {
        return None;
    }
    let mut pos = start + 6;
    for _ in 0..count {
        if pos + 3 > data.len() {
            return None;
        }
        let name_len = le16(&data[pos + 1..pos + 3]) as usize;
        pos += 3;
        if pos + name_len + 4 > data.len() {
            return None;
        }
        pos += name_len;
        let value_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if pos + value_len > data.len() {
            return None;
        }
        pos += value_len;
    }
    Some(pos - start)
}

fn ta_record_len(data: &[u8], start: usize) -> Option<usize> {
    if start + 8 > data.len() || !data[start..].starts_with(b"TA\x01\x01") {
        return None;
    }
    let count = u32::from_le_bytes([
        data[start + 4],
        data[start + 5],
        data[start + 6],
        data[start + 7],
    ]) as usize;
    if count == 0 || count > 100_000 {
        return None;
    }
    let mut pos = start + 8;
    for _ in 0..count {
        if pos + 5 > data.len() {
            return None;
        }
        pos += 1;
        let value_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if pos + value_len > data.len() {
            return None;
        }
        pos += value_len;
    }
    Some(pos - start)
}

fn parse_td_records(
    path: &str,
    data: &[u8],
    base_offset: usize,
    needles: &[String],
    show_all: bool,
    out: &mut Vec<RecordOut>,
) {
    let mut pos = 0usize;
    while pos + 6 <= data.len() {
        if data[pos..].starts_with(b"TA\x01\x01") {
            let Some(len) = ta_record_len(data, pos) else {
                pos += 1;
                continue;
            };
            parse_ta_record(
                path,
                &data[pos..pos + len],
                base_offset + pos,
                needles,
                show_all,
                out,
            );
            pos += len.max(1);
            continue;
        }
        if !data[pos..].starts_with(b"TD\x01\x01") {
            pos += 1;
            continue;
        }
        let Some(len) = td_record_len(data, pos) else {
            pos += 1;
            continue;
        };
        let rec = decode_txdata_fields(&data[pos + 4..pos + len]);
        let fields = rec
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| FieldOut {
                index,
                header: field.header,
                name: field.name.clone(),
                value_len: field.value_len,
                uint_value: uint_value(field),
                texts: field.value_texts.clone(),
                hex_prefix: hex_prefix(&field.value, 80),
            })
            .collect::<Vec<_>>();
        let hay = serde_json::to_string(&fields)
            .unwrap_or_default()
            .to_lowercase();
        let matched = needles
            .iter()
            .any(|needle| hay.contains(&needle.to_lowercase()));
        if show_all || matched {
            out.push(RecordOut {
                path: path.to_string(),
                offset: base_offset + pos,
                len,
                tag: "TD".to_string(),
                complete: rec.complete,
                matched,
                fields,
            });
        }
        for field in &rec.fields {
            parse_td_records(
                path,
                &field.value,
                base_offset + pos + 4,
                needles,
                show_all,
                out,
            );
        }
        pos += len.max(1);
    }
}

fn parse_ta_record(
    path: &str,
    data: &[u8],
    base_offset: usize,
    needles: &[String],
    show_all: bool,
    out: &mut Vec<RecordOut>,
) {
    if data.len() < 8 || !data.starts_with(b"TA\x01\x01") {
        return;
    }
    let count = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let mut pos = 8usize;
    for index in 0..count {
        if pos + 5 > data.len() {
            break;
        }
        let header = data[pos];
        let value_len =
            u32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]])
                as usize;
        pos += 5;
        if pos + value_len > data.len() {
            break;
        }
        let value = data[pos..pos + value_len].to_vec();
        let field = TxDataField {
            header,
            name: format!("array[{index}]"),
            value_len: value_len as u32,
            value: value.clone(),
            value_texts: Vec::new(),
            value_text_len: 0,
        };
        let fields = vec![FieldOut {
            index,
            header,
            name: field.name.clone(),
            value_len: field.value_len,
            uint_value: uint_value(&field),
            texts: field.value_texts.clone(),
            hex_prefix: hex_prefix(&field.value, 80),
        }];
        let hay = serde_json::to_string(&fields)
            .unwrap_or_default()
            .to_lowercase();
        let matched = needles
            .iter()
            .any(|needle| hay.contains(&needle.to_lowercase()));
        if show_all || matched {
            out.push(RecordOut {
                path: path.to_string(),
                offset: base_offset + pos - 5,
                len: value_len + 5,
                tag: "TA_ITEM".to_string(),
                complete: true,
                matched,
                fields,
            });
        }
        parse_td_records(path, &value, base_offset + pos, needles, show_all, out);
        pos += value_len;
    }
}

fn parse_es_plain(
    path: &str,
    data: &[u8],
    needles: &[String],
    show_all: bool,
    out: &mut Vec<RecordOut>,
) {
    if !data.starts_with(b"ES\x01\x01") {
        return;
    }
    let mut pos = 4usize;
    while pos + 5 <= data.len() {
        let tag = data[pos];
        let size = u32::from_be_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]])
            as usize;
        pos += 5;
        if size == 0 || pos + size > data.len() {
            break;
        }
        let payload = &data[pos..pos + size];
        if tag == 2 {
            parse_td_records(path, payload, pos, needles, show_all, out);
        }
        pos += size;
    }
}

fn main() -> anyhow::Result<()> {
    let mut show_all = false;
    let mut limit = 20usize;
    let mut paths = Vec::new();
    let mut needles = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => show_all = true,
            "--limit" => limit = args.next().context("--limit needs value")?.parse()?,
            "--needle" => needles.push(args.next().context("--needle needs value")?),
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    if paths.is_empty() {
        anyhow::bail!("usage: file_index_probe [--needle TEXT] [--all] PATH ...");
    }
    let mut out = Vec::new();
    for path in paths {
        let path_text = path.to_string_lossy().to_string();
        let data = fs::read(&path).with_context(|| path_text.clone())?;
        parse_td_records(&path_text, &data, 0, &needles, show_all, &mut out);
        parse_es_plain(&path_text, &data, &needles, show_all, &mut out);
    }
    if out.len() > limit {
        out.truncate(limit);
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

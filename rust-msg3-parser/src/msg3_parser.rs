use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};

use flate2::read::ZlibDecoder;
use serde_json::Value;

#[derive(Clone, Default)]
struct Field {
    id: u8,
    name: String,
    data: Vec<u8>,
    text: String,
}

#[derive(Clone, Default)]
struct TxField {
    header: u8,
    name: String,
    value_len: u32,
    value: Vec<u8>,
    value_texts: Vec<String>,
    value_text_len: usize,
}

#[derive(Clone, Default)]
struct TdRecord {
    offset: usize,
    tag: String,
    subtype: u16,
    field: u8,
    len: u16,
    len_fits: bool,
    data: Vec<u8>,
    trailer: Vec<u8>,
    low_bytes: Vec<u8>,
    decoded_texts: Vec<String>,
    txdata_field_count: u16,
    txdata_complete: bool,
    txdata_fields: Vec<TxField>,
    parse_errors: Vec<String>,
}

#[derive(Clone, Default)]
struct ProtoField {
    path: String,
    field: u64,
    wire: u64,
    varint: Option<u64>,
    bytes: Vec<u8>,
    text: String,
}

#[derive(Clone, Default)]
struct TlvRecord {
    source: String,
    offset: usize,
    tag: u8,
    name: String,
    data: Vec<u8>,
    proto_fields: Vec<ProtoField>,
}

#[derive(Clone, Default)]
struct Element {
    offset: usize,
    typ: u8,
    payload: Vec<u8>,
    fields: Vec<Field>,
    proto_strings: Vec<String>,
    proto_fields: Vec<ProtoField>,
    tlvs: Vec<TlvRecord>,
    markers: Vec<(usize, String)>,
    td_records: Vec<TdRecord>,
    summary: String,
}

fn le16(data: &[u8]) -> u16 {
    data[0] as u16 | ((data[1] as u16) << 8)
}

fn be16(data: &[u8]) -> u16 {
    ((data[0] as u16) << 8) | data[1] as u16
}

fn hex_bytes(data: &[u8], max_len: usize) -> String {
    let n = data.len().min(max_len);
    let mut out = String::new();
    for (i, b) in data[..n].iter().enumerate() {
        if i != 0 {
            out.push(' ');
        }
        out.push_str(&format!("{:02x}", b));
    }
    if data.len() > max_len {
        out.push_str(" ...");
    }
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn csv_escape(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn html_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn utf16le_to_string(data: &[u8]) -> String {
    let mut words = Vec::new();
    let mut i = 0;
    while i + 1 < data.len() {
        let w = le16(&data[i..i + 2]);
        if w == 0 {
            i += 2;
            continue;
        }
        if (0xd800..=0xdbff).contains(&w) {
            if i + 3 < data.len() {
                let w2 = le16(&data[i + 2..i + 4]);
                if (0xdc00..=0xdfff).contains(&w2) {
                    words.push(w);
                    words.push(w2);
                    i += 4;
                    continue;
                }
            }
            i += 2;
            continue;
        }
        if (0xdc00..=0xdfff).contains(&w) {
            i += 2;
            continue;
        }
        words.push(w);
        i += 2;
    }
    String::from_utf16_lossy(&words)
}

fn suspicious_score(text: &str) -> usize {
    text.chars()
        .map(|ch| {
            let c = ch as u32;
            if (0xe000..=0xf8ff).contains(&c) {
                1
            } else if (0xac00..=0xd7af).contains(&c)
                || (0x1200..=0x137f).contains(&c)
                || (0xa000..=0xa63f).contains(&c)
                || (0x2c80..=0x2cff).contains(&c)
            {
                1
            } else if c < 0x20 && ch != '\n' && ch != '\r' && ch != '\t' {
                1
            } else {
                0
            }
        })
        .sum()
}

fn reasonable_text(text: &str) -> bool {
    if text.is_empty() || text.len() > 4096 {
        return false;
    }
    let mut total = 0usize;
    let mut controls = 0usize;
    for ch in text.chars() {
        total += 1;
        if ch == '\t' || ch == '\n' || ch == '\r' {
            continue;
        }
        if (ch as u32) < 0x20 {
            controls += 1;
        }
    }
    if total == 0 || controls != 0 {
        return false;
    }
    suspicious_score(text) * 20 <= total
}

fn reasonable_field_text(text: &str) -> bool {
    if text.is_empty() || text.len() > 4096 {
        return false;
    }
    if text.contains('\u{fffd}') {
        return false;
    }
    let chars = text.chars().count().max(1);
    if suspicious_score(text) * 20 > chars {
        return false;
    }
    !text
        .chars()
        .any(|ch| (ch as u32) < 0x20 && ch != '\n' && ch != '\r' && ch != '\t')
}

fn valid_utf8_text(data: &[u8]) -> Option<String> {
    if data.is_empty() || data.iter().any(|b| *b == 0) {
        return None;
    }
    let text = std::str::from_utf8(data)
        .ok()?
        .trim_end_matches('\u{fffd}')
        .to_string();
    if text.contains('\u{fffd}') {
        return None;
    }
    if reasonable_text(&text) {
        Some(text)
    } else {
        None
    }
}

fn utf16le_text_runs(data: &[u8], min_units: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 1 < data.len() {
        let start = pos;
        let mut end = pos;
        let mut units = Vec::new();
        while end + 1 < data.len() {
            let w = le16(&data[end..end + 2]);
            let printable = w == 9
                || w == 10
                || w == 13
                || w == 0x20
                || (0x2d..=0xd7ff).contains(&w)
                || (0xe000..=0xfffd).contains(&w);
            if !printable || w == 0 {
                break;
            }
            units.push(w);
            end += 2;
        }
        if units.len() >= min_units {
            if let Ok(text) = String::from_utf16(&units) {
                let text = text.trim().to_string();
                if reasonable_field_text(&text) && !out.iter().any(|item| item == &text) {
                    out.push(text);
                }
            }
            pos = end.max(start + 2);
        } else {
            pos = start + 1;
        }
    }
    out
}

fn plausible_utf16le(data: &[u8]) -> bool {
    if data.len() < 2 || data.len() % 2 != 0 {
        return false;
    }
    let mut chars = 0usize;
    let mut printable = 0usize;
    let mut suspicious = 0usize;
    let mut i = 0;
    while i + 1 < data.len() {
        let w = le16(&data[i..i + 2]);
        if w != 0 {
            if (0xd800..=0xdbff).contains(&w) && i + 3 < data.len() {
                let w2 = le16(&data[i + 2..i + 4]);
                if (0xdc00..=0xdfff).contains(&w2) {
                    chars += 1;
                    printable += 1;
                    i += 4;
                    continue;
                }
            }
            chars += 1;
            if w == 9 || w == 10 || w == 13 || (0x20..0xd800).contains(&w) || w >= 0xe000 {
                printable += 1;
            }
            if (0xe000..=0xf8ff).contains(&w) || (0xac00..=0xd7af).contains(&w) {
                suspicious += 1;
            }
        }
        i += 2;
    }
    chars > 0 && printable * 100 / chars >= 90 && suspicious * 20 <= chars
}

fn plausible_ascii(data: &[u8]) -> bool {
    if data.len() < 3 {
        return false;
    }
    let good = data
        .iter()
        .filter(|b| **b == 9 || **b == 10 || **b == 13 || (0x20..=0x7e).contains(&**b))
        .count();
    good * 100 / data.len() >= 90
}

fn ascii_text(data: &[u8]) -> String {
    let bytes: Vec<u8> = data.iter().copied().filter(|b| *b != 0).collect();
    String::from_utf8_lossy(&bytes).to_string()
}

fn decoded_byte_texts(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(text) = valid_utf8_text(data) {
        out.push(text);
    }
    #[cfg(feature = "encoding_rs")]
    {
        let (decoded, _, had_errors) = encoding_rs::GB18030.decode(data);
        if !had_errors {
            let text = decoded.to_string();
            if reasonable_text(&text) && !is_generic_token(&text) && !out.contains(&text) {
                out.push(text);
            }
        }
    }
    if out.is_empty() && plausible_ascii(data) {
        let text = ascii_text(data);
        if reasonable_text(&text) {
            out.push(text);
        }
    }
    out
}

fn td_xor_high_byte(hi: u8) -> bool {
    (0x50..=0x7f).contains(&hi) || hi >= 0x80
}

fn txdata_lenkey_plain(data: &[u8]) -> Vec<u8> {
    if data.is_empty() || data.len() > 0xffff {
        return Vec::new();
    }
    let len = data.len() as u16;
    let key = (len as u8) ^ ((len >> 8) as u8);
    data.iter().map(|b| !*b ^ key).collect()
}

fn txdata_rich_utf16_text(data: &[u8]) -> String {
    let plain = txdata_lenkey_plain(data);
    if plain.len() < 2 || plain.len() % 2 != 0 {
        return String::new();
    }
    let mut units = Vec::with_capacity(plain.len() / 2);
    for chunk in plain.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            return String::new();
        }
        units.push(unit);
    }
    String::from_utf16(&units).unwrap_or_default()
}

fn txdata_visible_rich_text(data: &[u8]) -> String {
    let rich = txdata_rich_utf16_text(data);
    let mut text = String::new();
    let mut skip_next = false;
    for ch in rich.chars() {
        let c = ch as u32;
        if skip_next {
            skip_next = false;
            continue;
        }
        if c == 0x0014 {
            skip_next = true;
            continue;
        }
        if matches!(c, 0x0000..=0x001f | 0x0489) {
            continue;
        }
        text.push(ch);
    }
    if reasonable_field_text(&text) && !is_generic_token(&text) {
        text
    } else {
        String::new()
    }
}

fn td_xor_wide_texts(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < data.len() && out.len() < 128 {
        let hi = data[i + 1];
        if !td_xor_high_byte(hi) {
            i += 1;
            continue;
        }
        let mut added = false;
        let mut decoded = Vec::new();
        let mut j = i;
        while j + 1 < data.len() && data[j + 1] == hi {
            let c = data[j] ^ hi;
            if c == 0 || (c < 0x20 && c != 9 && c != 10 && c != 13) {
                break;
            }
            decoded.push(c);
            j += 2;
        }
        if decoded.len() >= 4 {
            for text in decoded_byte_texts(&decoded) {
                if !out.contains(&text) {
                    out.push(text);
                    added = true;
                }
            }
        }
        if added {
            i = j;
            continue;
        }

        if decoded.len() >= 4 && plausible_ascii(&decoded) {
            let text = ascii_text(&decoded);
            if reasonable_text(&text) && !is_generic_token(&text) && !out.contains(&text) {
                out.push(text);
                i = j;
                continue;
            }
        }
        {
            i += 1;
        }
    }
    out
}

fn read_varint(data: &[u8], off: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    for _ in 0..10 {
        if *off >= data.len() {
            return None;
        }
        let b = data[*off];
        *off += 1;
        value |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn is_generic_token(text: &str) -> bool {
    text.is_empty()
        || text == "platform"
        || text.starts_with("com.tencent.")
        || text.starts_with("UserDataImage:")
        || text.starts_with("UserDataCustomFace:")
        || text.starts_with("UserDataRoot:")
        || text.starts_with("FaceStore:")
        || text.starts_with("GlobalSysTemp:")
        || text.starts_with("sysres:")
}

fn collect_proto_strings(data: &[u8], depth: usize, out: &mut Vec<String>) {
    if depth > 2 || data.len() < 2 || out.len() > 200 {
        return;
    }
    let mut off = 0usize;
    let mut fields_seen = 0usize;
    while off < data.len() && fields_seen < 80 {
        let field_start = off;
        let key = match read_varint(data, &mut off) {
            Some(v) if v != 0 => v,
            _ => return,
        };
        let wire = key & 7;
        match wire {
            0 => {
                if read_varint(data, &mut off).is_none() {
                    return;
                }
            }
            1 => {
                if off + 8 > data.len() {
                    return;
                }
                off += 8;
            }
            2 => {
                let n = match read_varint(data, &mut off) {
                    Some(v) => v as usize,
                    None => return,
                };
                if off + n > data.len() {
                    return;
                }
                let bytes = &data[off..off + n];
                if (3..=300).contains(&n) {
                    if let Some(text) = valid_utf8_text(bytes) {
                        if !is_generic_token(&text) {
                            out.push(text);
                        }
                    }
                }
                collect_proto_strings(bytes, depth + 1, out);
                off += n;
            }
            5 => {
                if off + 4 > data.len() {
                    return;
                }
                off += 4;
            }
            _ => {
                let _ = field_start;
                return;
            }
        }
        fields_seen += 1;
    }
}

fn looks_like_proto(data: &[u8]) -> bool {
    if data.len() < 2 {
        return false;
    }
    let mut off = 0usize;
    let mut fields = 0usize;
    while off < data.len() && fields < 12 {
        let key = match read_varint(data, &mut off) {
            Some(v) if v != 0 => v,
            _ => return false,
        };
        let field = key >> 3;
        let wire = key & 7;
        if field == 0 || field > 0x10000 {
            return false;
        }
        match wire {
            0 => {
                if read_varint(data, &mut off).is_none() {
                    return false;
                }
            }
            1 => off = off.saturating_add(8),
            2 => {
                let n = match read_varint(data, &mut off) {
                    Some(v) => v as usize,
                    None => return false,
                };
                off = off.saturating_add(n);
            }
            5 => off = off.saturating_add(4),
            _ => return false,
        }
        if off > data.len() {
            return false;
        }
        fields += 1;
    }
    fields > 0 && off == data.len()
}

fn collect_proto_fields(data: &[u8], depth: usize, path: &str, out: &mut Vec<ProtoField>) {
    if depth > 3 || data.len() < 2 || out.len() > 500 || !looks_like_proto(data) {
        return;
    }
    let mut off = 0usize;
    while off < data.len() && out.len() < 500 {
        let key = match read_varint(data, &mut off) {
            Some(v) if v != 0 => v,
            _ => return,
        };
        let field_no = key >> 3;
        let wire = key & 7;
        let cur = if path.is_empty() {
            field_no.to_string()
        } else {
            format!("{}.{}", path, field_no)
        };
        let mut item = ProtoField {
            path: cur.clone(),
            field: field_no,
            wire,
            ..Default::default()
        };
        match wire {
            0 => item.varint = read_varint(data, &mut off),
            1 => {
                if off + 8 > data.len() {
                    return;
                }
                item.bytes = data[off..off + 8].to_vec();
                off += 8;
            }
            2 => {
                let n = match read_varint(data, &mut off) {
                    Some(v) => v as usize,
                    None => return,
                };
                if off + n > data.len() {
                    return;
                }
                item.bytes = data[off..off + n].to_vec();
                if n <= 500 {
                    if let Some(text) = valid_utf8_text(&item.bytes) {
                        item.text = text;
                    }
                }
                if n <= 4096 {
                    collect_proto_fields(&item.bytes, depth + 1, &cur, out);
                }
                off += n;
            }
            5 => {
                if off + 4 > data.len() {
                    return;
                }
                item.bytes = data[off..off + 4].to_vec();
                off += 4;
            }
            _ => return,
        }
        out.push(item);
    }
}

fn tlv_tag_name(tag: u8) -> String {
    match tag {
        0x1d => "resv3.ResvAttr".to_string(),
        0x21 => "resv6.ResvAttr".to_string(),
        _ => String::new(),
    }
}

fn collect_ctx_tlvs(data: &[u8], source: &str, out: &mut Vec<TlvRecord>) {
    let mut off = 0usize;
    let mut seen = 0usize;
    while off + 3 <= data.len() && seen < 160 && out.len() < 200 {
        let tag = data[off];
        if tag == 0xff {
            return;
        }
        let n = be16(&data[off + 1..off + 3]) as usize;
        if n > data.len() - off - 3 {
            return;
        }
        let payload = data[off + 3..off + 3 + n].to_vec();
        let name = tlv_tag_name(tag);
        let mut rec = TlvRecord {
            source: source.to_string(),
            offset: off,
            tag,
            name,
            data: payload,
            ..Default::default()
        };
        if (tag == 0x1d || tag == 0x21) && !rec.data.is_empty() {
            let base = format!("{}.tlv{}", source, tag);
            collect_proto_fields(&rec.data, 0, &base, &mut rec.proto_fields);
        }
        if n > 0 && (!rec.name.is_empty() || !rec.proto_fields.is_empty()) {
            out.push(rec);
        }
        off += 3 + n;
        seen += 1;
    }
}

fn field_name(elem_type: u8, field_id: u8) -> String {
    match (elem_type, field_id) {
        (1, 1) => "text",
        (2, 1) => "face_id_or_text",
        (3 | 5 | 6, 0) => "rich_or_resv_data",
        (3 | 5 | 6, 1) => "guid_or_md5",
        (3 | 5 | 6, 2) => "image_path",
        (3 | 5 | 6, 8) => "image_biz_type",
        (3 | 5 | 6, 10) => "image_extra",
        (3 | 5 | 6, 11) => "image_size_or_flag",
        (7 | 0x11, 0) => "rich_or_struct_data",
        (7 | 0x11, 1) => "guid",
        (7 | 0x11, 2) => "app_or_service",
        (7 | 0x11, 3) => "custom_data",
        (0x08, 1) => "metadata_flag",
        (0x08, 2) => "metadata_value",
        (0x0c, 1) => "display_text",
        (0x0c, 2) => "rich_context_data",
        (0x0e, 1) => "layout_flag",
        (0x12, 2) => "text_metadata",
        (0x14, 1) => "xml",
        (0x14, 2) => "abstract_text",
        (0x19, 1) => "common_elem",
        (0x19, 2) => "custom_data",
        _ => "",
    }
    .to_string()
}

fn valid_element_type(t: u8) -> bool {
    matches!(
        t,
        1 | 2
            | 3
            | 5
            | 6
            | 7
            | 0x08
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
    )
}

fn element_label(t: u8) -> &'static str {
    match t {
        1 => "text",
        2 => "sysface",
        3 | 5 | 6 => "image",
        7 | 0x11 => "file_or_struct",
        0x08 => "message_metadata",
        0x0c => "rich_context",
        0x0d => "emoji",
        0x0e => "layout_metadata",
        0x10 => "public_account",
        0x12 => "text_metadata",
        0x14 => "special_abstract",
        0x15 | 0x16 | 0x18 => "custom_data",
        0x19 => "custom_abstract",
        0x1a => "video",
        0x1b => "ark_app",
        0x1e => "multi_msg",
        _ => "unknown",
    }
}

fn parse_fields(elem_type: u8, payload: &[u8]) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut off = 0usize;
    while off + 3 <= payload.len() {
        let id = payload[off];
        let n = le16(&payload[off + 1..off + 3]) as usize;
        if n > payload.len() - off - 3 {
            break;
        }
        let data = payload[off + 3..off + 3 + n].to_vec();
        let name = field_name(elem_type, id);
        let allow_text = !(matches!(elem_type, 3 | 5 | 6) && id != 2);
        let mut text = String::new();
        if elem_type == 1 && id == 1 && data.len() >= 2 && data.len() % 2 == 0 {
            let candidate = utf16le_to_string(&data)
                .trim_start_matches(|ch: char| {
                    (ch as u32) < 0x20 && ch != '\n' && ch != '\r' && ch != '\t'
                })
                .to_string();
            if !candidate.is_empty()
                && !candidate
                    .chars()
                    .any(|ch| (ch as u32) < 0x20 && ch != '\n' && ch != '\r' && ch != '\t')
                && !candidate.contains('\u{fffd}')
            {
                text = candidate;
            }
        } else if allow_text && plausible_utf16le(&data) {
            let candidate = utf16le_to_string(&data);
            if reasonable_field_text(&candidate) {
                text = candidate;
            }
        } else if allow_text && plausible_ascii(&data) {
            let candidate = ascii_text(&data);
            if reasonable_field_text(&candidate) {
                text = candidate;
            }
        }
        fields.push(Field {
            id,
            name,
            data,
            text,
        });
        off += 3 + n;
    }
    fields
}

fn utf16le_embedded_strings(data: &[u8], min_units: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start + 1 < data.len() {
        let mut pos = start;
        let mut units = Vec::new();
        while pos + 1 < data.len() {
            let w = le16(&data[pos..pos + 2]);
            let printable =
                w == 9 || w == 10 || w == 13 || (0x20..0xd800).contains(&w) || w >= 0xe000;
            if !printable {
                break;
            }
            units.push(w);
            pos += 2;
        }
        if units.len() >= min_units {
            if let Ok(text) = String::from_utf16(&units) {
                let text = text.trim_matches(char::from(0)).trim().to_string();
                if !text.is_empty() && !out.iter().any(|(_, item)| item == &text) {
                    out.push((start, text));
                }
            }
            start = pos.max(start + 2);
        } else {
            start += 1;
        }
    }
    out
}

fn embedded_visible_elements(data: &[u8]) -> Vec<Element> {
    let mut out = Vec::new();
    for (offset, text) in utf16le_embedded_strings(data, 5) {
        let image_text = ["UserDataImage:", "UserDataCustomFace:"]
            .iter()
            .filter_map(|needle| text.find(needle).map(|idx| text[idx..].to_string()))
            .next()
            .map(|s| trim_media_path(&s))
            .unwrap_or_else(|| trim_media_path(&text));
        if image_text.starts_with("UserDataImage:")
            || image_text.starts_with("UserDataCustomFace:")
            || image_text.contains("\\Image")
            || image_text.to_ascii_lowercase().ends_with(".jpg")
            || image_text.to_ascii_lowercase().ends_with(".jpeg")
            || image_text.to_ascii_lowercase().ends_with(".png")
            || image_text.to_ascii_lowercase().ends_with(".gif")
            || image_text.to_ascii_lowercase().ends_with(".webp")
        {
            let data: Vec<u8> = image_text
                .encode_utf16()
                .flat_map(|w| w.to_le_bytes())
                .collect();
            out.push(Element {
                offset,
                typ: 6,
                payload: data.clone(),
                fields: vec![Field {
                    id: 2,
                    name: "image_path".to_string(),
                    data,
                    text: image_text,
                }],
                summary: "[图片]".to_string(),
                ..Default::default()
            });
        } else if text.starts_with("UserDataRoot:Video")
            || text.to_ascii_lowercase().contains("\\video\\")
        {
            out.push(Element {
                offset,
                typ: 0x1a,
                payload: text.encode_utf16().flat_map(|w| w.to_le_bytes()).collect(),
                fields: vec![Field {
                    id: 1,
                    name: "embedded_video_path".to_string(),
                    data: text.encode_utf16().flat_map(|w| w.to_le_bytes()).collect(),
                    text,
                }],
                summary: "[视频]".to_string(),
                ..Default::default()
            });
        } else if text.starts_with("UserDataFileRecv:") || text.contains("FileRecv") {
            out.push(Element {
                offset,
                typ: 7,
                payload: text.encode_utf16().flat_map(|w| w.to_le_bytes()).collect(),
                fields: vec![Field {
                    id: 3,
                    name: "embedded_file_path".to_string(),
                    data: text.encode_utf16().flat_map(|w| w.to_le_bytes()).collect(),
                    text,
                }],
                summary: "[文件]".to_string(),
                ..Default::default()
            });
        }
    }
    out
}

fn trim_media_path(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    for ext in [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".mp4", ".webm", ".mov", ".amr", ".mp3",
        ".wav", ".ogg", ".m4a",
    ] {
        if let Some(idx) = lower.find(ext) {
            let end = idx + ext.len();
            return text[..end].to_string();
        }
    }
    text.to_string()
}

fn find_markers(data: &[u8]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for i in 0..data.len().saturating_sub(3) {
        if marker_record_len(data, i).is_some() {
            out.push((i, String::from_utf8_lossy(&data[i..i + 2]).to_string()));
        }
    }
    out
}

fn td_low_byte_candidate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < data.len() {
        let hi = data[i + 1];
        if (0x60..=0x70).contains(&hi)
            || matches!(hi, 0xcf | 0xdd | 0xdf | 0xe3 | 0xe5 | 0xe9 | 0xeb | 0xef)
        {
            out.push(data[i]);
        }
        i += 2;
    }
    out
}

fn parse_txdata_fields(data: &[u8], rec: &mut TdRecord) -> bool {
    let decoded = crate::txdata_codec::decode_txdata_fields(data);
    rec.txdata_field_count = decoded.count;
    rec.txdata_complete = decoded.complete;
    rec.txdata_fields = decoded
        .fields
        .into_iter()
        .map(|field| TxField {
            header: field.header,
            name: field.name,
            value_len: field.value_len,
            value: field.value,
            value_texts: field.value_texts,
            value_text_len: field.value_text_len,
        })
        .collect();
    if !rec.txdata_fields.is_empty() && !rec.txdata_complete {
        rec.parse_errors.push(format!(
            "incomplete TXData record: declared_field_count={}, parsed_field_count={}, input_len={}; structured rich parsing will not consume this partial record",
            rec.txdata_field_count,
            rec.txdata_fields.len(),
            data.len()
        ));
    }
    !rec.txdata_fields.is_empty()
}

fn td_record_len(data: &[u8], start: usize) -> Option<usize> {
    if start + 6 > data.len() || !data[start..].starts_with(b"TD\x01\x01") {
        return None;
    }
    let count = le16(&data[start + 4..start + 6]) as usize;
    if count == 0 {
        return Some(6);
    }
    if count > 200 {
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
    if count > 100_000 {
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

fn marker_record_len(data: &[u8], start: usize) -> Option<usize> {
    if start + 4 > data.len() {
        return None;
    }
    if data[start..].starts_with(b"TD\x01\x01") {
        td_record_len(data, start)
    } else if data[start..].starts_with(b"TA\x01\x01") {
        ta_record_len(data, start)
    } else {
        None
    }
}

fn parse_td_ta_records_at(data: &[u8], base_offset: usize, depth: usize, out: &mut Vec<TdRecord>) {
    if depth > 8 || out.len() >= 400 {
        return;
    }
    let mut pos = 0usize;
    while pos + 4 <= data.len() && out.len() < 400 {
        if data[pos..].starts_with(b"TD\x01\x01") {
            let Some(len) = td_record_len(data, pos) else {
                pos += 1;
                continue;
            };
            let mut rec = TdRecord {
                offset: base_offset + pos,
                tag: "TD".to_string(),
                len_fits: true,
                data: data[pos + 4..pos + len].to_vec(),
                ..Default::default()
            };
            parse_txdata_fields(&data[pos + 4..pos + len], &mut rec);
            for field in &rec.txdata_fields {
                parse_td_ta_records_at(&field.value, base_offset + pos + 4, depth + 1, out);
            }
            rec.low_bytes = td_low_byte_candidate(&rec.data);
            if rec.txdata_fields.is_empty() {
                rec.decoded_texts = td_xor_wide_texts(&rec.data);
            }
            out.push(rec);
            pos += len.max(1);
            continue;
        }
        if data[pos..].starts_with(b"TA\x01\x01") {
            let Some(len) = ta_record_len(data, pos) else {
                pos += 1;
                continue;
            };
            let mut rec = TdRecord {
                offset: base_offset + pos,
                tag: "TA".to_string(),
                len_fits: true,
                txdata_complete: true,
                data: data[pos + 4..pos + len].to_vec(),
                ..Default::default()
            };
            let count =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
            rec.txdata_field_count = count.min(u16::MAX as u32) as u16;
            out.push(rec);

            let mut item_pos = pos + 8;
            for index in 0..count as usize {
                if item_pos + 5 > pos + len {
                    break;
                }
                let header = data[item_pos];
                let value_len = u32::from_le_bytes([
                    data[item_pos + 1],
                    data[item_pos + 2],
                    data[item_pos + 3],
                    data[item_pos + 4],
                ]) as usize;
                let value_start = item_pos + 5;
                let value_end = value_start + value_len;
                if value_end > pos + len {
                    break;
                }
                let mut item = TdRecord {
                    offset: base_offset + item_pos,
                    tag: "TA_ITEM".to_string(),
                    field: header,
                    len: value_len.min(u16::MAX as usize) as u16,
                    len_fits: true,
                    txdata_complete: true,
                    data: data[value_start..value_end].to_vec(),
                    ..Default::default()
                };
                item.decoded_texts = td_xor_wide_texts(&item.data);
                item.low_bytes = td_low_byte_candidate(&item.data);
                out.push(item);
                parse_td_ta_records_at(
                    &data[value_start..value_end],
                    base_offset + value_start,
                    depth + 1,
                    out,
                );
                let _ = index;
                item_pos = value_end;
            }
            pos += len.max(1);
            continue;
        }
        pos += 1;
    }
}

fn parse_td_records(data: &[u8]) -> Vec<TdRecord> {
    let mut out = Vec::new();
    parse_td_ta_records_at(data, 0, 0, &mut out);
    out
}

fn txdata_numeric_name(name: &str) -> bool {
    name.starts_with("dw")
        || name.starts_with('n')
        || name.starts_with('b')
        || name.starts_with('c')
        || name.starts_with('w')
        || name.starts_with('u')
}

fn starts_with_td_ta(data: &[u8]) -> bool {
    data.len() >= 4
        && data[0] == b'T'
        && (data[1] == b'D' || data[1] == b'A')
        && data[2] == 1
        && data[3] == 1
}

fn near_td_ta_marker_delta(data: &[u8], max_delta: usize) -> Option<usize> {
    let limit = data.len().saturating_sub(3).min(max_delta + 1);
    for delta in 1..limit {
        if starts_with_td_ta(&data[delta..]) {
            return Some(delta);
        }
    }
    None
}

fn looks_like_image_path(text: &str) -> bool {
    text.starts_with("UserDataImage:")
        || text.starts_with("UserDataCustomFace:")
        || text.starts_with("FaceStore:")
        || text.starts_with("GlobalSysTemp:")
        || text.starts_with("sysres:")
        || text.contains("\\Image")
        || text.contains("/Image")
        || text.contains(".jpg")
        || text.contains(".gif")
        || text.contains(".png")
        || text.contains(".bmp")
}

fn is_bluebar_sysres_path(text: &str) -> bool {
    let normalized = text.trim().replace('/', "\\").to_ascii_lowercase();
    normalized.starts_with("sysres:misc\\bluebar\\")
}

fn is_system_decoration_element(e: &Element) -> bool {
    matches!(e.typ, 3 | 5 | 6)
        && e.fields
            .iter()
            .any(|field| field.name == "image_path" && is_bluebar_sysres_path(&field.text))
}

fn face_text(id: u8) -> String {
    const FACES: &[&str] = &[
        "微笑",
        "撇嘴",
        "色",
        "发呆",
        "得意",
        "流泪",
        "害羞",
        "闭嘴",
        "睡",
        "大哭",
        "尴尬",
        "发怒",
        "调皮",
        "呲牙",
        "惊讶",
        "难过",
        "酷",
        "冷汗",
        "抓狂",
        "吐",
        "偷笑",
        "可爱",
        "白眼",
        "傲慢",
        "饥饿",
        "困",
        "惊恐",
        "流汗",
        "憨笑",
        "大兵",
        "奋斗",
        "咒骂",
        "疑问",
        "嘘",
        "晕",
        "折磨",
        "衰",
        "骷髅",
        "敲打",
        "再见",
        "擦汗",
        "抠鼻",
        "鼓掌",
        "糗大了",
        "坏笑",
        "左哼哼",
        "右哼哼",
        "哈欠",
        "鄙视",
        "委屈",
        "快哭了",
        "阴险",
        "亲亲",
        "吓",
        "可怜",
        "菜刀",
        "西瓜",
        "啤酒",
        "篮球",
        "乒乓",
        "咖啡",
        "饭",
        "猪头",
        "玫瑰",
        "凋谢",
        "示爱",
        "爱心",
        "心碎",
        "蛋糕",
        "闪电",
        "炸弹",
        "刀",
        "足球",
        "瓢虫",
        "便便",
        "月亮",
        "太阳",
        "礼物",
        "拥抱",
        "强",
        "弱",
        "握手",
        "胜利",
        "抱拳",
        "勾引",
        "拳头",
        "差劲",
        "爱你",
        "NO",
        "OK",
        "爱情",
        "飞吻",
        "跳跳",
        "发抖",
        "怄火",
        "转圈",
        "磕头",
        "回头",
        "跳绳",
        "挥手",
        "激动",
        "街舞",
        "献吻",
        "左太极",
        "右太极",
    ];
    if let Some(name) = FACES.get(id as usize) {
        format!("[{}]", name)
    } else if let Some(shortcut) = extended_face_shortcut(id) {
        format!("[{}]", shortcut)
    } else {
        format!("[face:{}]", id)
    }
}

fn extended_face_shortcut(id: u8) -> Option<&'static str> {
    match id {
        // Verified in IM.dll static shortcut table: PTR_DAT_562c3b00[212] -> L"/qw".
        212 => Some("/qw"),
        _ => None,
    }
}

fn sysface_id(e: &Element) -> Option<u8> {
    if e.typ == 2 && e.payload.len() >= 4 && e.payload[0] == 1 && e.payload[1] == 1 {
        Some(e.payload[3])
    } else {
        None
    }
}

fn is_qq_face_label(text: &str) -> bool {
    let trimmed = text.trim();
    (0..=100).any(|id| face_text(id) == trimmed)
}

fn first_meaningful_text(e: &Element, max_len: usize) -> String {
    for f in &e.fields {
        if !f.text.is_empty()
            && f.text.len() <= max_len
            && !f.text.contains('\u{fffd}')
            && !is_generic_token(&f.text)
        {
            return f.text.clone();
        }
    }
    String::new()
}

fn first_proto_text(e: &Element, max_len: usize) -> String {
    for tlv in &e.tlvs {
        for pf in &tlv.proto_fields {
            if !pf.text.is_empty() && pf.text.len() <= max_len && !is_generic_token(&pf.text) {
                return pf.text.clone();
            }
        }
    }
    for pf in &e.proto_fields {
        if !pf.text.is_empty() && pf.text.len() <= max_len && !is_generic_token(&pf.text) {
            return pf.text.clone();
        }
    }
    String::new()
}

fn summarize_element(e: &Element) -> String {
    if e.typ == 1 {
        let link = first_txdata_named_text(e, &["strLinkHttp"]);
        if !link.is_empty() {
            return link;
        }
        for f in &e.fields {
            if f.id == 1 && !f.text.is_empty() && !f.text.contains('\u{fffd}') {
                return f.text.clone();
            }
        }
        if e.fields
            .iter()
            .any(|f| f.id == 1 && f.text.contains('\u{fffd}'))
        {
            return "[无法解码文本]".to_string();
        }
    }
    if e.typ == 2 && e.payload.len() >= 4 {
        let link = first_txdata_named_text(e, &["strLinkHttp"]);
        if !link.is_empty() {
            return link;
        }
        if e.payload[0] == 1 && e.payload[1] == 1 && e.payload[2] == 0 {
            return face_text(e.payload[3]);
        }
        let mut text = first_meaningful_text(e, 80);
        if !text.is_empty() {
            if text.starts_with('/') {
                text.remove(0);
            }
            return format!("[{}]", text);
        }
        return String::new();
    }
    if matches!(e.typ, 3 | 5 | 6) {
        if is_system_decoration_element(e) {
            return String::new();
        }
        let proto_text = first_proto_text(e, 80);
        if !proto_text.is_empty() {
            return proto_text;
        }
        let mut has_image = false;
        for f in &e.fields {
            if !f.text.is_empty() && !looks_like_image_path(&f.text) && f.text.len() < 80 {
                return f.text.clone();
            }
            if looks_like_image_path(&f.text) {
                has_image = true;
            }
        }
        if starts_with_td_ta(&e.payload) || has_image {
            return "[图片]".to_string();
        }
    }
    if e.typ == 0x0c {
        return rich_context_text(e);
    }
    if e.typ == 0x0d {
        return first_meaningful_text(e, 32);
    }
    if e.typ == 7 || e.typ == 0x11 {
        let text = first_meaningful_text(e, 120);
        if !text.is_empty() {
            return text;
        }
        let text = structured_txdata_display_text(e);
        if !text.is_empty() {
            return text;
        }
        for f in &e.fields {
            if f.text == "com.tencent.filetransfer" {
                return "[文件]".to_string();
            }
        }
        return String::new();
    }
    if e.typ == 0x10 {
        return first_meaningful_text(e, 120);
    }
    if e.typ == 0x14 {
        let xml = struct_msg_xml(e);
        let text = struct_msg_display_text(&xml, &first_meaningful_text(e, 160));
        if !text.is_empty() {
            return text;
        }
        return first_proto_text(e, 160);
    }
    if matches!(e.typ, 0x15 | 0x16 | 0x18 | 0x19) {
        let text = first_meaningful_text(e, 160);
        if !text.is_empty() {
            return text;
        }
        return first_proto_text(e, 160);
    }
    if e.typ == 0x1a {
        return "[视频]".to_string();
    }
    if e.typ == 0x1b {
        let text = ark_app_display_text(e);
        if !text.is_empty() {
            return text;
        }
        return first_meaningful_text(e, 160);
    }
    if e.typ == 0x1e {
        let text = multi_msg_display_text(e);
        if !text.is_empty() {
            return text;
        }
        return first_meaningful_text(e, 160);
    }
    String::new()
}

fn useful_element(e: &Element) -> bool {
    if e.typ == 1 && e.summary.is_empty() && e.fields.is_empty() {
        return false;
    }
    !e.summary.is_empty()
        || (e.typ == 0 && e.fields.iter().any(|f| f.name.starts_with("raw_")))
        || (e.typ == 1 && !e.fields.is_empty())
        || (e.typ == 0x08 && !e.fields.is_empty())
        || (e.typ == 0x0c && e.payload.len() >= 8)
        || !e.tlvs.is_empty()
        || !e.td_records.is_empty()
        || (e.typ == 0x19 && (!e.fields.is_empty() || !e.proto_fields.is_empty()))
        || matches!(e.typ, 0x0e | 0x12)
}

fn exact_utf16le_text(data: &[u8]) -> String {
    if data.len() < 2 || data.len() % 2 != 0 {
        return String::new();
    }
    if data.len() >= 4 {
        let hi = data[1];
        if td_xor_high_byte(hi)
            && data.chunks_exact(2).all(|chunk| chunk[1] == hi)
            && !td_xor_wide_texts(data).is_empty()
        {
            return String::new();
        }
    }
    let units = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    let Ok(text) = String::from_utf16(&units) else {
        return String::new();
    };
    if text.chars().any(|ch| !ch.is_ascii())
        && reasonable_field_text(&text)
        && !is_generic_token(&text)
    {
        text
    } else {
        String::new()
    }
}

fn standard_element_len_at(data: &[u8], pos: usize) -> Option<usize> {
    if pos + 4 <= data.len() && data[pos..].starts_with(b"TD\x01\x01") {
        return td_record_len(data, pos);
    }
    if pos + 4 <= data.len() && data[pos..].starts_with(b"TA\x01\x01") {
        return ta_record_len(data, pos);
    }
    if pos + 3 > data.len() {
        return None;
    }
    let typ = data[pos];
    let n = le16(&data[pos + 1..pos + 3]) as usize;
    let zero_len_metadata = n == 0 && matches!(typ, 0x0e | 0x12);
    if valid_element_type(typ) && (zero_len_metadata || n >= 3) && n <= data.len() - pos - 3 {
        Some(3 + n)
    } else {
        None
    }
}

fn next_standard_element_start(data: &[u8], pos: usize, max_scan: usize) -> Option<usize> {
    let end = data.len().min(pos.saturating_add(max_scan));
    let mut scan = pos;
    while scan + 3 <= end {
        if standard_element_len_at(data, scan).is_some() {
            return Some(scan);
        }
        scan += 1;
    }
    if end == data.len() {
        Some(data.len())
    } else {
        None
    }
}

fn raw_gap_element(data: &[u8], pos: usize, end: usize) -> Option<Element> {
    if pos < 32 || end <= pos || end - pos > 512 {
        return None;
    }
    let payload = data[pos..end].to_vec();
    let visible = exact_utf16le_text(&payload);
    let mut fields = Vec::new();
    if !visible.is_empty() {
        fields.push(Field {
            id: 0,
            name: "raw_text_gap".to_string(),
            data: payload.clone(),
            text: visible.clone(),
        });
    }
    for text in td_xor_wide_texts(&payload) {
        if !fields.iter().any(|f| f.text == text) {
            fields.push(Field {
                id: 0,
                name: "raw_text_gap".to_string(),
                data: payload.clone(),
                text,
            });
        }
    }
    if fields.is_empty() {
        let parsed = parse_fields(0, &payload);
        let consumed = parsed.iter().map(|f| 3 + f.data.len()).sum::<usize>();
        if consumed == payload.len() && !parsed.is_empty() {
            for mut field in parsed {
                field.name = "raw_field_gap".to_string();
                fields.push(field);
            }
        }
    }
    if fields.is_empty() {
        return None;
    }
    Some(Element {
        offset: pos,
        typ: 0,
        payload,
        fields,
        summary: visible,
        ..Default::default()
    })
}

fn parse_msgcontent(data: &[u8]) -> Vec<Element> {
    let mut out = Vec::new();
    let mut pos = if data.len() >= 20 && data.starts_with(b"MSG") {
        20
    } else {
        0
    };
    while pos + 3 <= data.len() {
        let raw_td_len = if pos + 4 <= data.len() && data[pos..].starts_with(b"TD\x01\x01") {
            td_record_len(data, pos)
        } else if pos + 4 <= data.len() && data[pos..].starts_with(b"TA\x01\x01") {
            ta_record_len(data, pos)
        } else {
            None
        };
        if let Some(n) = raw_td_len {
            let payload = data[pos..pos + n].to_vec();
            let mut e = Element {
                offset: pos,
                typ: 0,
                payload,
                ..Default::default()
            };
            e.markers = find_markers(&e.payload);
            e.td_records = parse_td_records(&e.payload);
            e.summary = summarize_element(&e);
            if useful_element(&e) {
                out.push(e);
            }
            pos += n.max(1);
            continue;
        }
        let typ = data[pos];
        let n = le16(&data[pos + 1..pos + 3]) as usize;
        if typ == 1 && n >= 1024 {
            if let Some(delta) = near_td_ta_marker_delta(&data[pos..], 8) {
                pos += delta;
                continue;
            }
        }
        let zero_len_metadata = n == 0 && matches!(typ, 0x0e | 0x12);
        if !valid_element_type(typ) || (!zero_len_metadata && n < 3) || n > data.len() - pos - 3 {
            if let Some(next) = next_standard_element_start(data, pos + 1, 1024) {
                if let Some(e) = raw_gap_element(data, pos, next) {
                    out.push(e);
                    pos = next;
                    continue;
                }
            }
            pos += 1;
            continue;
        }
        let payload = data[pos + 3..pos + 3 + n].to_vec();
        let raw_td = starts_with_td_ta(&payload);
        let mut e = Element {
            offset: pos,
            typ,
            payload,
            ..Default::default()
        };
        if !raw_td {
            e.fields = parse_fields(typ, &e.payload);
        }
        collect_proto_strings(&e.payload, 0, &mut e.proto_strings);
        collect_proto_fields(&e.payload, 0, "", &mut e.proto_fields);
        collect_ctx_tlvs(&e.payload, "payload", &mut e.tlvs);
        for f in &e.fields {
            let base = if f.name.is_empty() {
                format!("field{}", f.id)
            } else {
                f.name.clone()
            };
            collect_proto_strings(&f.data, 0, &mut e.proto_strings);
            collect_proto_fields(&f.data, 0, &base, &mut e.proto_fields);
            collect_ctx_tlvs(&f.data, &base, &mut e.tlvs);
        }
        e.markers = find_markers(&e.payload);
        if raw_td || !e.markers.is_empty() {
            e.td_records = parse_td_records(&e.payload);
        }
        e.summary = summarize_element(&e);
        if useful_element(&e) {
            out.push(e);
            pos += 3 + n;
        } else if let Some(raw) = raw_gap_element(data, pos, pos + 3 + n) {
            out.push(raw);
            pos += 3 + n;
        } else {
            pos += 1;
        }
    }
    if !out.iter().any(|e| !e.summary.is_empty()) && transport_only_label(&out).is_empty() {
        out.extend(embedded_visible_elements(data));
    }
    out
}

fn element_has_field_named(e: &Element, name: &str) -> bool {
    e.fields.iter().any(|f| f.name == name)
}

fn element_display_status(elems: &[Element], idx: usize, skip: &HashSet<usize>) -> &'static str {
    let e = &elems[idx];
    if is_mmt_transport_index(e) {
        return if embedded_mmt_elements(e).is_empty() {
            "unclassified"
        } else {
            "rendered"
        };
    }
    if skip.contains(&idx) {
        return "ignored";
    }
    if is_common_elem_style_metadata(e) {
        return "ignored";
    }
    if is_system_decoration_element(e) {
        return "ignored";
    }
    if matches!(e.typ, 0x08 | 0x0e | 0x12) || element_has_field_named(e, "raw_field_gap") {
        return "ignored";
    }
    if element_has_field_named(e, "raw_text_gap") {
        return if e.summary.is_empty() {
            "ignored"
        } else {
            "rendered"
        };
    }
    if !e.summary.is_empty() || !element_candidate_texts(e).is_empty() {
        return "rendered";
    }
    if e.typ == 0x19 && !new_sys_face_resource_id(e).is_empty() {
        return "rendered";
    }
    "unclassified"
}

fn elements_json(elems: &[Element]) -> String {
    let mut out = String::from("[");
    let skip = display_skip_indices(elems);
    for (i, e) in elems.iter().enumerate() {
        if i != 0 {
            out.push(',');
        }
        let display_status = element_display_status(elems, i, &skip);
        out.push_str(&format!(
            "{{\"offset\":{},\"type\":{},\"kind\":\"{}\",\"payload_len\":{},\"display_status\":\"{}\"",
            e.offset,
            e.typ,
            element_label(e.typ),
            e.payload.len(),
            display_status
        ));
        if !e.summary.is_empty() {
            out.push_str(&format!(",\"summary\":\"{}\"", json_escape(&e.summary)));
        }
        if !e.proto_fields.is_empty() {
            out.push_str(",\"proto_fields\":[");
            for (j, pf) in e.proto_fields.iter().enumerate() {
                if j != 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    "{{\"path\":\"{}\",\"field\":{},\"wire\":{}",
                    json_escape(&pf.path),
                    pf.field,
                    pf.wire
                ));
                if let Some(v) = pf.varint {
                    out.push_str(&format!(",\"varint\":{}", v));
                }
                if !pf.bytes.is_empty() {
                    out.push_str(&format!(
                        ",\"len\":{},\"hex\":\"{}\"",
                        pf.bytes.len(),
                        hex_bytes(&pf.bytes, 64)
                    ));
                }
                if !pf.text.is_empty() {
                    out.push_str(&format!(",\"text\":\"{}\"", json_escape(&pf.text)));
                }
                out.push('}');
            }
            out.push(']');
        }
        if !e.proto_strings.is_empty() {
            out.push_str(",\"proto_strings\":[");
            for (j, text) in e.proto_strings.iter().enumerate() {
                if j != 0 {
                    out.push(',');
                }
                out.push_str(&format!("\"{}\"", json_escape(text)));
            }
            out.push(']');
        }
        if !e.markers.is_empty() {
            out.push_str(",\"markers\":[");
            for (j, (offset, tag)) in e.markers.iter().enumerate() {
                if j != 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    "{{\"offset\":{},\"tag\":\"{}\"}}",
                    offset,
                    json_escape(tag)
                ));
            }
            out.push(']');
        }
        if !e.tlvs.is_empty() {
            out.push_str(",\"ctx_tlvs\":[");
            for (j, tlv) in e.tlvs.iter().enumerate() {
                if j != 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    "{{\"source\":\"{}\",\"offset\":{},\"tag\":{},\"len\":{}",
                    json_escape(&tlv.source),
                    tlv.offset,
                    tlv.tag,
                    tlv.data.len()
                ));
                if !tlv.name.is_empty() {
                    out.push_str(&format!(",\"name\":\"{}\"", json_escape(&tlv.name)));
                }
                out.push_str(&format!(",\"hex\":\"{}\"", hex_bytes(&tlv.data, 64)));
                if !tlv.proto_fields.is_empty() {
                    out.push_str(",\"proto_fields\":[");
                    for (k, pf) in tlv.proto_fields.iter().enumerate() {
                        if k != 0 {
                            out.push(',');
                        }
                        out.push_str(&format!(
                            "{{\"path\":\"{}\",\"field\":{},\"wire\":{}",
                            json_escape(&pf.path),
                            pf.field,
                            pf.wire
                        ));
                        if let Some(v) = pf.varint {
                            out.push_str(&format!(",\"varint\":{}", v));
                        }
                        if !pf.bytes.is_empty() {
                            out.push_str(&format!(
                                ",\"len\":{},\"hex\":\"{}\"",
                                pf.bytes.len(),
                                hex_bytes(&pf.bytes, 64)
                            ));
                        }
                        if !pf.text.is_empty() {
                            out.push_str(&format!(",\"text\":\"{}\"", json_escape(&pf.text)));
                        }
                        out.push('}');
                    }
                    out.push(']');
                }
                out.push('}');
            }
            out.push(']');
        }
        if !e.td_records.is_empty() {
            out.push_str(",\"td_ta_records\":[");
            for (j, rec) in e.td_records.iter().enumerate() {
                if j != 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    "{{\"offset\":{},\"tag\":\"{}\",\"subtype\":{},\"field\":{},\"len\":{},\"len_fits\":{}",
                    rec.offset, rec.tag, rec.subtype, rec.field, rec.len, rec.len_fits
                ));
                if !rec.decoded_texts.is_empty() {
                    out.push_str(",\"decoded_texts\":[");
                    for (k, text) in rec.decoded_texts.iter().enumerate() {
                        if k != 0 {
                            out.push(',');
                        }
                        out.push_str(&format!("\"{}\"", json_escape(text)));
                    }
                    out.push(']');
                }
                if !rec.low_bytes.is_empty() {
                    out.push_str(&format!(
                        ",\"low_bytes_hex\":\"{}\"",
                        hex_bytes(&rec.low_bytes, 64)
                    ));
                    if plausible_ascii(&rec.low_bytes) {
                        let text = ascii_text(&rec.low_bytes);
                        if reasonable_text(&text) {
                            out.push_str(&format!(
                                ",\"low_bytes_text\":\"{}\"",
                                json_escape(&text)
                            ));
                        }
                    }
                }
                if !rec.data.is_empty() {
                    out.push_str(&format!(",\"data_hex\":\"{}\"", hex_bytes(&rec.data, 64)));
                }
                if !rec.trailer.is_empty() {
                    out.push_str(&format!(
                        ",\"trailer_len\":{},\"trailer_hex\":\"{}\"",
                        rec.trailer.len(),
                        hex_bytes(&rec.trailer, 64)
                    ));
                }
                if !rec.parse_errors.is_empty() {
                    out.push_str(",\"parse_errors\":[");
                    for (k, err) in rec.parse_errors.iter().enumerate() {
                        if k != 0 {
                            out.push(',');
                        }
                        out.push_str(&format!("\"{}\"", json_escape(err)));
                    }
                    out.push(']');
                }
                if !rec.txdata_fields.is_empty() {
                    out.push_str(&format!(
                        ",\"txdata_field_count\":{},\"txdata_complete\":{},\"txdata_fields\":[",
                        rec.txdata_field_count, rec.txdata_complete
                    ));
                    for (k, f) in rec.txdata_fields.iter().enumerate() {
                        if k != 0 {
                            out.push(',');
                        }
                        out.push_str(&format!(
                            "{{\"header\":{},\"name\":\"{}\",\"value_len\":{},\"value_hex\":\"{}\"",
                            f.header,
                            json_escape(&f.name),
                            f.value_len,
                            hex_bytes(&f.value, 96)
                        ));
                        if matches!(f.value.len(), 1 | 2 | 4 | 8) && txdata_numeric_name(&f.name) {
                            let mut value = 0u64;
                            for (idx, b) in f.value.iter().enumerate() {
                                value |= (*b as u64) << (8 * idx);
                            }
                            out.push_str(&format!(",\"value_uint\":{}", value));
                        }
                        if !f.value_texts.is_empty() {
                            out.push_str(",\"value_texts\":[");
                            for (n, text) in f.value_texts.iter().enumerate() {
                                if n != 0 {
                                    out.push(',');
                                }
                                out.push_str(&format!("\"{}\"", json_escape(text)));
                            }
                            out.push(']');
                            if f.value_text_len > 0 && f.value_text_len <= f.value.len() {
                                out.push_str(&format!(",\"value_text_len\":{}", f.value_text_len));
                                if f.value_text_len < f.value.len() {
                                    out.push_str(&format!(
                                        ",\"value_unparsed_hex\":\"{}\"",
                                        hex_bytes(&f.value[f.value_text_len..], 96)
                                    ));
                                }
                            }
                        }
                        if matches!(
                            f.name.as_str(),
                            "bsAbstractText" | "MSGBOX_bsPreviewMsgText" | "bsMsgText"
                        ) && !f.value.is_empty()
                        {
                            let rich_text = txdata_rich_utf16_text(&f.value);
                            if !rich_text.is_empty() {
                                out.push_str(&format!(
                                    ",\"value_rich_utf16\":\"{}\"",
                                    json_escape(&rich_text)
                                ));
                            }
                            let visible_text = txdata_visible_rich_text(&f.value);
                            if !visible_text.is_empty() {
                                out.push_str(&format!(
                                    ",\"value_visible_text\":\"{}\"",
                                    json_escape(&visible_text)
                                ));
                            }
                        }
                        out.push('}');
                    }
                    out.push(']');
                }
                out.push('}');
            }
            out.push(']');
        }
        out.push_str(",\"fields\":[");
        for (j, f) in e.fields.iter().enumerate() {
            if j != 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"id\":{},\"len\":{},\"hex\":\"{}\"",
                f.id,
                f.data.len(),
                hex_bytes(&f.data, 64)
            ));
            if !f.name.is_empty() {
                out.push_str(&format!(",\"name\":\"{}\"", json_escape(&f.name)));
            }
            if !f.text.is_empty() {
                out.push_str(&format!(",\"text\":\"{}\"", json_escape(&f.text)));
            }
            out.push('}');
        }
        out.push_str(&format!(
            "],\"payload_hex\":\"{}\"}}",
            hex_bytes(&e.payload, 64)
        ));
    }
    out.push(']');
    out
}

fn is_reply_abstract(e: &Element) -> bool {
    e.typ == 0x19
        && (e
            .proto_fields
            .iter()
            .any(|field| field.path.starts_with("common_elem.45"))
            || e.proto_strings.iter().any(|text| text.contains("聊天记录")))
}

fn is_common_elem_face(e: &Element) -> bool {
    e.typ == 0x19
        && !is_reply_abstract(e)
        && e.proto_fields
            .iter()
            .any(|field| field.path.starts_with("common_elem.53"))
        && !e.summary.trim().is_empty()
}

fn is_common_elem_style_metadata(e: &Element) -> bool {
    e.typ == 0x19
        && e.summary.trim().is_empty()
        && new_sys_face_resource_id(e).is_empty()
        && element_candidate_texts(e).is_empty()
        && e.proto_fields
            .iter()
            .any(|field| field.path.starts_with("common_elem.37"))
}

fn new_sys_face_resource_id(e: &Element) -> String {
    if e.typ != 0x19 || is_reply_abstract(e) {
        return String::new();
    }
    if !e
        .proto_fields
        .iter()
        .any(|field| field.path.starts_with("common_elem.37"))
    {
        return String::new();
    }
    for text in &e.proto_strings {
        if text.starts_with("u_") && text.chars().count() >= 8 {
            return text.clone();
        }
    }
    String::new()
}

fn is_mmt_transport_index(e: &Element) -> bool {
    let mut has_struct_version = false;
    let mut has_index = false;
    let mut has_packets = false;
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for field in &rec.txdata_fields {
            match field.name.as_str() {
                "bStructMsgVersion" => has_struct_version = true,
                "arrMMTMsgIndex" => has_index = true,
                "arrPackets" => has_packets = true,
                _ => {}
            }
        }
    }
    has_struct_version && has_index && has_packets
}

fn embedded_mmt_elements(e: &Element) -> Vec<Element> {
    if !is_mmt_transport_index(e) {
        return Vec::new();
    }
    parse_msgcontent(&e.payload)
        .into_iter()
        .filter(|child| !(child.offset == 0 && is_mmt_transport_index(child)))
        .collect()
}

fn embedded_payload_elements(e: &Element) -> Vec<Element> {
    if e.typ == 1 || e.payload.len() < 6 {
        return Vec::new();
    }
    if has_transport_metadata(e) {
        return Vec::new();
    }
    if is_common_elem_style_metadata(e)
        || matches!(e.typ, 0 | 0x08 | 0x0c | 0x0e | 0x12 | 0x14 | 0x18)
    {
        return Vec::new();
    }
    if !e.summary.trim().is_empty() && !is_stable_media_candidate(&e.summary) {
        return Vec::new();
    }
    let children = parse_msgcontent(&e.payload);
    if children.is_empty() {
        return Vec::new();
    }
    let visible = children
        .iter()
        .map(|child| child.summary.as_str())
        .collect::<String>();
    if visible.trim().is_empty() || visible.trim() == e.summary.trim() {
        Vec::new()
    } else {
        children
    }
}

fn append_element_bytes(out: &mut Vec<u8>, e: &Element) {
    if e.typ == 0 {
        out.extend_from_slice(&e.payload);
    } else if e.payload.len() <= u16::MAX as usize {
        out.push(e.typ);
        out.extend_from_slice(&(e.payload.len() as u16).to_le_bytes());
        out.extend_from_slice(&e.payload);
    }
}

fn mmt_display_elements(elems: &[Element], idx: usize) -> Vec<Element> {
    let mut stream = elems[idx].payload.clone();
    for e in elems.iter().skip(idx + 1) {
        append_element_bytes(&mut stream, e);
    }
    parse_msgcontent(&stream)
        .into_iter()
        .filter(|child| !(child.offset == 0 && is_mmt_transport_index(child)))
        .collect()
}

fn has_transport_metadata(e: &Element) -> bool {
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for field in &rec.txdata_fields {
            match field.name.as_str() {
                "arrPackets" | "C2CSessionData" | "dwConnTime" | "dwServerTime" | "dwMsgSeq"
                | "dwBeginSeq" | "dwEndSeq" | "dwPacketSeq" | "dwPacketCount" | "dwPacketIndex"
                | "nSessionType" => return true,
                _ => {}
            }
        }
    }
    false
}

fn has_unresolved_visible_txdata_text(e: &Element) -> bool {
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for field in &rec.txdata_fields {
            if matches!(
                field.name.as_str(),
                "bsAbstractText" | "MSGBOX_bsPreviewMsgText" | "bsMsgText"
            ) && field.value_texts.is_empty()
                && !field.value.is_empty()
            {
                return true;
            }
        }
    }
    false
}

fn has_txdata_field(e: &Element, name: &str) -> bool {
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        if rec.txdata_fields.iter().any(|field| field.name == name) {
            return true;
        }
    }
    false
}

fn has_system_abstract_text(e: &Element) -> bool {
    e.typ == 0 && has_txdata_field(e, "bsAbstractText")
}

fn system_message_label(elems: &[Element]) -> String {
    if elems.iter().any(has_unresolved_visible_txdata_text) {
        if elems.iter().any(has_system_abstract_text) {
            return "[系统消息]".to_string();
        }
        panic!("unresolved visible TXData text without a known system-message behavior");
    }
    String::new()
}

fn av_call_label(e: &Element) -> String {
    let mut has_av_field = false;
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for field in &rec.txdata_fields {
            match field.name.as_str() {
                "wAvMsgType" | "bVideo" | "nMsgContentType" => has_av_field = true,
                "bsMsgText" => {
                    for text in &field.value_texts {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() && !is_generic_token(trimmed) {
                            return trimmed.to_string();
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if has_av_field {
        "[音视频通话]".to_string()
    } else {
        String::new()
    }
}

fn transport_only_label(elems: &[Element]) -> String {
    if elems.iter().any(is_mmt_transport_index) {
        "[无可见正文：MMT包索引]".to_string()
    } else if let Some(label) = elems
        .iter()
        .map(av_call_label)
        .find(|label| !label.is_empty())
    {
        label
    } else {
        let label = system_message_label(elems);
        if !label.is_empty() {
            return label;
        }
        if elems.iter().any(has_transport_metadata) {
            "[无可见正文：传输元数据]".to_string()
        } else {
            String::new()
        }
    }
}

fn is_reply_leadin_text(e: &Element) -> bool {
    if e.typ != 1 {
        return false;
    }
    let text = e.summary.trim();
    text.is_empty() || (text.starts_with('@') && text.chars().count() <= 64)
}

fn is_reply_mention_text(e: &Element) -> bool {
    e.typ == 1 && e.summary.trim().starts_with('@') && e.summary.trim().chars().count() <= 64
}

fn is_inline_mention_suffix(e: &Element) -> bool {
    matches!(e.typ, 2 | 0x0d) && !e.summary.is_empty()
}

fn mention_with_inline_suffix(
    elems: &[Element],
    idx: usize,
    skip: &HashSet<usize>,
    consumed: &mut HashSet<usize>,
) -> String {
    let mut text = elems[idx].summary.clone();
    if !is_reply_mention_text(&elems[idx]) {
        return text;
    }
    let mut next = idx + 1;
    while next < elems.len() {
        if skip.contains(&next)
            || consumed.contains(&next)
            || !is_inline_mention_suffix(&elems[next])
        {
            break;
        }
        text.push_str(&elems[next].summary);
        consumed.insert(next);
        next += 1;
    }
    text
}

fn is_space_text(e: &Element) -> bool {
    e.typ == 1 && e.summary.trim().is_empty()
}

fn quote_text_score(text: &str) -> i32 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return -1000;
    }
    let mut score = trimmed.chars().count() as i32;
    for ch in trimmed.chars() {
        if ch.is_control() {
            score -= 80;
        }
    }
    if trimmed.starts_with("u_") && trimmed.chars().count() <= 32 {
        score -= 200;
    }
    if trimmed.starts_with('\n') {
        score -= 60;
    }
    if trimmed.contains("聊天记录") {
        score += 20;
    }
    score
}

fn reply_quote_text(e: &Element) -> String {
    let mut best = String::new();
    let mut best_score = -1000;
    for pf in &e.proto_fields {
        if !pf.path.starts_with("common_elem.45") || pf.text.trim().is_empty() {
            continue;
        }
        let score = quote_text_score(&pf.text) + pf.path.matches('.').count() as i32 * 3;
        if score > best_score {
            best_score = score;
            best = pf.text.clone();
        }
    }
    if !best.trim().is_empty() {
        return best.trim().to_string();
    }
    for text in &e.proto_strings {
        let score = quote_text_score(text);
        if score > best_score {
            best_score = score;
            best = text.clone();
        }
    }
    if !best.trim().is_empty() {
        return best.trim().to_string();
    }
    e.summary.trim().to_string()
}

fn reply_quote_href(e: &Element) -> String {
    for pf in &e.proto_fields {
        if !pf.path.starts_with("common_elem.45") {
            continue;
        }
        let text = pf.text.trim();
        if text.starts_with("http://") || text.starts_with("https://") {
            return text.to_string();
        }
    }
    for text in &e.proto_strings {
        let trimmed = text.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return trimmed.to_string();
        }
    }
    String::new()
}

fn display_skip_indices(elems: &[Element]) -> HashSet<usize> {
    let mut skip = HashSet::new();
    for (idx, elem) in elems.iter().enumerate() {
        if !is_reply_abstract(elem) {
            continue;
        }
        let mut prev = idx;
        while prev > 0 {
            prev -= 1;
            if !is_reply_leadin_text(&elems[prev]) {
                break;
            }
            skip.insert(prev);
        }
        let first = idx + 1;
        if first < elems.len() && is_reply_mention_text(&elems[first]) {
            let mut next = first + 1;
            while next < elems.len() && is_space_text(&elems[next]) {
                next += 1;
            }
            if next < elems.len()
                && is_reply_mention_text(&elems[next])
                && elems[first].summary.trim() == elems[next].summary.trim()
            {
                skip.insert(first);
                for space_idx in first + 1..next {
                    skip.insert(space_idx);
                }
            }
        }
    }
    skip
}

fn plain_text(elems: &[Element]) -> String {
    let mut out = String::new();
    let skip = display_skip_indices(elems);
    for (idx, e) in elems.iter().enumerate() {
        if skip.contains(&idx) {
            continue;
        }
        if is_system_decoration_element(e) {
            continue;
        }
        if (e.typ == 0x0c && e.summary.is_empty())
            || (e.typ == 0x19 && !is_common_elem_face(e) && new_sys_face_resource_id(e).is_empty())
        {
            continue;
        }
        if e.typ == 0x19 && e.summary.is_empty() {
            let res_id = new_sys_face_resource_id(e);
            if !res_id.is_empty() {
                out.push_str("[新系统表情]");
            }
        } else if is_mmt_transport_index(e) {
            let children = mmt_display_elements(elems, idx);
            if !children.is_empty() {
                out.push_str(&plain_text(&children));
                break;
            } else {
                out.push_str(&e.summary);
            }
        } else {
            let children = embedded_payload_elements(e);
            if !children.is_empty() {
                out.push_str(&plain_text(&children));
            } else if e.summary.is_empty() {
                let candidates = element_candidate_texts(e);
                if let Some(url) = candidates
                    .iter()
                    .find(|text| text.starts_with("http://") || text.starts_with("https://"))
                {
                    out.push_str(url);
                }
            } else {
                out.push_str(&e.summary);
            }
        }
    }
    if out.is_empty() {
        out.push_str(&transport_only_label(elems));
    }
    out
}

fn is_stable_media_candidate(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if is_bluebar_sysres_path(trimmed) {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    let has_ext = [
        ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".amr", ".mp3", ".wav", ".ogg", ".m4a",
        ".mp4", ".webm", ".mov", ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".txt", ".apk", ".zip",
        ".rar", ".7z",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext));
    trimmed.starts_with("UserDataImage:")
        || trimmed.starts_with("UserDataCustomFace:")
        || trimmed.starts_with("UserDataRoot:")
        || trimmed.starts_with("OSRoot:")
        || trimmed.starts_with("UserSysWinTemp:")
        || trimmed.starts_with("FaceStore:")
        || trimmed.starts_with("GlobalSysTemp:")
        || trimmed.starts_with("sysres:")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.contains("\\Image")
        || trimmed.contains("/Image")
        || trimmed.contains("FileRecv")
        || ((trimmed.contains('\\') || trimmed.contains('/')) && has_ext)
        || (trimmed.chars().all(|ch| !ch.is_control()) && has_ext)
}

fn push_unique_text(out: &mut Vec<String>, text: &str) {
    if !text.is_empty() && !out.iter().any(|item| item == text) {
        out.push(text.to_string());
    }
}

fn element_candidate_texts(e: &Element) -> Vec<String> {
    let mut out = Vec::new();
    for f in &e.fields {
        if is_stable_media_candidate(&f.text) {
            push_unique_text(&mut out, &f.text);
        }
    }
    for text in &e.proto_strings {
        if is_stable_media_candidate(text) {
            push_unique_text(&mut out, text);
        }
    }
    for pf in &e.proto_fields {
        if is_stable_media_candidate(&pf.text) {
            push_unique_text(&mut out, &pf.text);
        }
    }
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for text in &rec.decoded_texts {
            if is_stable_media_candidate(text) {
                push_unique_text(&mut out, text);
            }
        }
        for f in &rec.txdata_fields {
            for text in &f.value_texts {
                if is_stable_media_candidate(text) {
                    push_unique_text(&mut out, text);
                }
            }
        }
    }
    out
}

fn txdata_named_texts(e: &Element, names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for field in &rec.txdata_fields {
            if !names.iter().any(|name| *name == field.name) {
                continue;
            }
            for text in &field.value_texts {
                push_unique_text(&mut out, text);
            }
        }
    }
    out
}

fn first_txdata_named_text(e: &Element, names: &[&str]) -> String {
    txdata_named_texts(e, names)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn first_txdata_named_uint(e: &Element, names: &[&str]) -> Option<u64> {
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for field in &rec.txdata_fields {
            if !names.iter().any(|name| *name == field.name) {
                continue;
            }
            if matches!(field.value.len(), 1 | 2 | 4 | 8) {
                let mut value = 0u64;
                for (idx, b) in field.value.iter().enumerate() {
                    value |= (*b as u64) << (8 * idx);
                }
                return Some(value);
            }
        }
    }
    None
}

fn structured_txdata_display_text(e: &Element) -> String {
    for xml in txdata_named_texts(e, &["strXmlInfo"]) {
        let text = struct_msg_display_text(&xml, "");
        if !text.trim().is_empty() && !is_generic_token(text.trim()) {
            return text.trim().to_string();
        }
    }

    let title = first_txdata_named_text(e, &["strTitle"]);
    let summary = first_txdata_named_text(e, &["strSummary"]);
    match (!title.trim().is_empty(), !summary.trim().is_empty()) {
        (true, true) => format!("{} {}", title.trim(), summary.trim()),
        (true, false) => title.trim().to_string(),
        (false, true) => summary.trim().to_string(),
        (false, false) => String::new(),
    }
}

fn has_txdata_named_field(e: &Element, names: &[&str]) -> bool {
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for field in &rec.txdata_fields {
            if names.iter().any(|name| *name == field.name) {
                return true;
            }
        }
    }
    false
}

fn link_texts(e: &Element) -> Vec<String> {
    txdata_named_texts(
        e,
        &["strLinkHttp", "strMsgLink", "strLinkUrl", "c_strMsgLinkUrl"],
    )
}

fn rich_link_node_json(text: &str, hrefs: &[String]) -> String {
    let href = hrefs.first().map(String::as_str).unwrap_or("");
    let mut node = format!(
        "{{\"type\":\"rich\",\"text\":\"{}\",\"href\":\"{}\"",
        json_escape(text),
        json_escape(href)
    );
    if hrefs.len() > 1 {
        node.push_str(",\"hrefs\":[");
        for (i, href) in hrefs.iter().enumerate() {
            if i != 0 {
                node.push(',');
            }
            node.push_str(&format!("\"{}\"", json_escape(href)));
        }
        node.push(']');
    }
    node.push('}');
    node
}

fn video_candidate_texts(e: &Element) -> Vec<String> {
    let mut out = txdata_named_texts(e, &["strVideoFilePath", "strVideoThumbPath"]);
    for text in element_candidate_texts(e) {
        let lower = text.to_ascii_lowercase();
        if lower.contains("\\video\\")
            || lower.contains("/video/")
            || lower.ends_with(".mp4")
            || lower.ends_with(".webm")
            || lower.ends_with(".mov")
        {
            push_unique_text(&mut out, &text);
        }
    }
    out
}

fn file_transfer_candidates(e: &Element) -> Vec<String> {
    let mut candidates = element_candidate_texts(e);
    for pf in &e.proto_fields {
        let text = pf.text.trim();
        if text.is_empty() || text == "{}" || text.ends_with("Byte") {
            continue;
        }
        if text.starts_with('/')
            || text.starts_with('\\')
            || text.chars().all(|ch| ch.is_ascii_hexdigit()) && text.len() >= 16
        {
            push_unique_text(&mut candidates, text);
            push_unique_text(&mut candidates, &format!("UserDataFileRecv:{}", text));
            if !e.summary.is_empty() && (text.starts_with('/') || text.starts_with('\\')) {
                push_unique_text(
                    &mut candidates,
                    &format!("UserDataFileRecv:{}\\{}", text, e.summary),
                );
            }
        }
    }
    if is_stable_media_candidate(&e.summary) {
        push_unique_text(&mut candidates, &e.summary);
        push_unique_text(&mut candidates, &format!("UserDataFileRecv:{}", e.summary));
    }
    candidates
}

fn looks_like_file_name(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    [
        ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".pdf", ".txt", ".zip", ".rar", ".7z",
        ".apk", ".exe", ".msi", ".jpg", ".jpeg", ".png", ".gif", ".webp", ".mp3", ".wav", ".m4a",
        ".mp4", ".mov", ".avi",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn file_transfer_meta_json(e: &Element) -> String {
    let mut file_name = if looks_like_file_name(&e.summary) {
        e.summary.trim().to_string()
    } else {
        String::new()
    };
    let mut size_text = String::new();
    let mut size_value = 0u64;
    let mut uuid = String::new();
    let mut hash = String::new();
    for pf in &e.proto_fields {
        let text = pf.text.trim();
        if !text.is_empty() {
            if file_name.is_empty() && looks_like_file_name(text) {
                file_name = text.to_string();
            }
            if size_text.is_empty()
                && text.ends_with("Byte")
                && text[..text.len().saturating_sub(4)]
                    .chars()
                    .all(|ch| ch.is_ascii_digit())
            {
                size_text = text.to_string();
                size_value = text[..text.len() - 4].parse::<u64>().unwrap_or(0);
            }
            if uuid.is_empty() && (text.starts_with('/') || text.starts_with('\\')) {
                uuid = text.to_string();
            }
            if hash.is_empty() && text.len() >= 16 && text.chars().all(|ch| ch.is_ascii_hexdigit())
            {
                hash = text.to_string();
            }
        }
        if size_value == 0 {
            if let Some(value) = pf.varint {
                if (1024..(1u64 << 43)).contains(&value) {
                    size_value = value;
                }
            }
        }
    }
    if size_text.is_empty() && size_value > 0 {
        size_text = format!("{}Byte", size_value);
    }
    let mut out = String::from("{");
    let mut first = true;
    for (key, value) in [
        ("file_name", file_name),
        ("size_text", size_text),
        ("uuid", uuid),
        ("hash", hash),
    ] {
        if value.is_empty() {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&format!("\"{}\":\"{}\"", key, json_escape(&value)));
    }
    if size_value > 0 {
        if !first {
            out.push(',');
        }
        out.push_str(&format!("\"size\":{}", size_value));
    }
    out.push('}');
    out
}

fn media_node_kind(candidates: &[String]) -> &'static str {
    for text in candidates {
        let lower = text.to_ascii_lowercase();
        if lower.ends_with(".amr")
            || lower.ends_with(".mp3")
            || lower.ends_with(".wav")
            || lower.ends_with(".ogg")
            || lower.ends_with(".m4a")
            || lower.contains("\\audio\\")
            || lower.contains("/audio/")
        {
            return "voice";
        }
    }
    for text in candidates {
        let lower = text.to_ascii_lowercase();
        if lower.ends_with(".mp4")
            || lower.ends_with(".webm")
            || lower.ends_with(".mov")
            || lower.contains("\\video\\")
            || lower.contains("/video/")
        {
            return "video";
        }
    }
    for text in candidates {
        let lower = text.to_ascii_lowercase();
        if lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".png")
            || lower.ends_with(".gif")
            || lower.ends_with(".webp")
            || lower.ends_with(".bmp")
            || text.starts_with("UserDataImage:")
            || text.starts_with("UserDataCustomFace:")
            || text.starts_with("FaceStore:")
            || text.starts_with("GlobalSysTemp:")
            || text.starts_with("sysres:")
        {
            return "image";
        }
    }
    "file"
}

fn rich_context_text(e: &Element) -> String {
    for field in &e.fields {
        if field.name == "display_text" {
            let trimmed = field.text.trim();
            if !trimmed.is_empty() && reasonable_field_text(trimmed) {
                return trimmed.to_string();
            }
        }
    }
    if !e.fields.is_empty() {
        return String::new();
    }
    let runs = utf16le_text_runs(&e.payload, 8);
    let mut useful = Vec::new();
    for text in runs {
        let trimmed = text.trim();
        if trimmed.is_empty()
            || trimmed.contains("<$\u{1ff}")
            || trimmed
                .chars()
                .all(|ch| ch.is_ascii_punctuation() || ch.is_whitespace())
        {
            continue;
        }
        if trimmed.chars().count() < 8 {
            continue;
        }
        useful.push(trimmed.to_string());
    }
    useful.join("\n")
}

fn title_list_preview(e: &Element, max_items: usize) -> Vec<String> {
    let mut out = Vec::new();
    for rec in &e.td_records {
        if !rec.txdata_complete {
            continue;
        }
        for field in &rec.txdata_fields {
            if field.name != "titleList" {
                continue;
            }
            let mut pos = 0usize;
            let data = &field.value;
            while pos + 8 <= data.len() && out.len() < max_items {
                if !data[pos..].starts_with(b"TA\x01\x01") {
                    pos += 1;
                    continue;
                }
                let Some(len) = ta_record_len(data, pos) else {
                    pos += 1;
                    continue;
                };
                let count = u32::from_le_bytes([
                    data[pos + 4],
                    data[pos + 5],
                    data[pos + 6],
                    data[pos + 7],
                ]) as usize;
                let mut item_pos = pos + 8;
                for _ in 0..count {
                    if item_pos + 5 > pos + len || out.len() >= max_items {
                        break;
                    }
                    let header = data[item_pos];
                    let value_len = u32::from_le_bytes([
                        data[item_pos + 1],
                        data[item_pos + 2],
                        data[item_pos + 3],
                        data[item_pos + 4],
                    ]) as usize;
                    let value_start = item_pos + 5;
                    let value_end = value_start + value_len;
                    if value_end > pos + len {
                        break;
                    }
                    if header == 8 {
                        let text = utf16le_to_string(&data[value_start..value_end])
                            .trim()
                            .to_string();
                        if reasonable_field_text(&text) {
                            push_unique_text(&mut out, &text);
                        }
                    }
                    item_pos = value_end;
                }
                pos += len.max(1);
            }
        }
    }
    out
}

fn multi_msg_display_text(e: &Element) -> String {
    let title = first_txdata_named_text(e, &["title"]);
    let brief = first_txdata_named_text(e, &["brief"]);
    if !title.trim().is_empty() && !brief.trim().is_empty() {
        format!("{} {}", brief.trim(), title.trim())
    } else if !title.trim().is_empty() {
        title.trim().to_string()
    } else if !brief.trim().is_empty() {
        brief.trim().to_string()
    } else {
        String::new()
    }
}

fn first_multi_msg_named_text(e: &Element, name: &str) -> String {
    first_txdata_named_text(e, &[name])
}

fn field_text(e: &Element, id: u8) -> String {
    for f in &e.fields {
        if f.id == id && !f.text.is_empty() {
            return f.text.clone();
        }
    }
    String::new()
}

fn extract_xml_attr(xml: &str, attr: &str) -> String {
    let needle = format!("{}=\"", attr);
    if let Some(start) = xml.find(&needle) {
        let value_start = start + needle.len();
        if let Some(end) = xml[value_start..].find('"') {
            return xml[value_start..value_start + end].to_string();
        }
    }
    let needle = format!("{}='", attr);
    if let Some(start) = xml.find(&needle) {
        let value_start = start + needle.len();
        if let Some(end) = xml[value_start..].find('\'') {
            return xml[value_start..value_start + end].to_string();
        }
    }
    String::new()
}

fn zlib_inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

fn decode_struct_msg_payload(data: &[u8]) -> String {
    let candidates: [&[u8]; 2] = if data.len() > 1 && data[0] <= 1 {
        [&data[1..], data]
    } else {
        [data, &[]]
    };
    for candidate in candidates {
        if candidate.len() < 2 || candidate[0] != 0x78 {
            continue;
        }
        let Some(inflated) = zlib_inflate(candidate) else {
            continue;
        };
        if let Ok(text) = String::from_utf8(inflated.clone()) {
            if reasonable_field_text(&text) {
                return text;
            }
        }
        let text = utf16le_to_string(&inflated);
        if reasonable_field_text(&text) {
            return text;
        }
    }
    String::new()
}

fn struct_msg_xml(e: &Element) -> String {
    for f in &e.fields {
        let text = decode_struct_msg_payload(&f.data);
        if !text.is_empty() {
            return text;
        }
    }
    decode_struct_msg_payload(&e.payload)
}

fn struct_msg_display_text(xml: &str, fallback: &str) -> String {
    for attr in ["brief", "m_resid", "title", "summary"] {
        let text = extract_xml_attr(xml, attr);
        if !text.trim().is_empty() {
            return text.trim().to_string();
        }
    }
    if !fallback.trim().is_empty() {
        fallback.trim().to_string()
    } else if !xml.trim().is_empty() {
        "[结构消息]".to_string()
    } else {
        String::new()
    }
}

fn find_json_string(value: &Value, key: &str) -> String {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(text)) = map.get(key) {
                if !text.trim().is_empty() {
                    return text.trim().to_string();
                }
            }
            for child in map.values() {
                let text = find_json_string(child, key);
                if !text.is_empty() {
                    return text;
                }
            }
            String::new()
        }
        Value::Array(items) => {
            for child in items {
                let text = find_json_string(child, key);
                if !text.is_empty() {
                    return text;
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn ark_meta_json(e: &Element) -> Option<Value> {
    for text in txdata_named_texts(e, &["meta", "config-json"]) {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            return Some(value);
        }
    }
    None
}

fn ark_app_display_text(e: &Element) -> String {
    let prompt = first_txdata_named_text(e, &["prompt"]);
    if !prompt.trim().is_empty() {
        return prompt.trim().to_string();
    }
    if let Some(meta) = ark_meta_json(e) {
        for key in ["title", "desc", "tag"] {
            let text = find_json_string(&meta, key);
            if !text.is_empty() {
                return text;
            }
        }
    }
    first_txdata_named_text(e, &["desc", "view", "app"])
}

fn ark_app_href(e: &Element) -> String {
    if let Some(meta) = ark_meta_json(e) {
        for key in ["jumpUrl", "source_url", "url"] {
            let text = find_json_string(&meta, key);
            if text.starts_with("http://") || text.starts_with("https://") {
                return text;
            }
        }
    }
    String::new()
}

fn append_json_array_items(out: &mut String, first: &mut bool, array_json: &str) {
    let trimmed = array_json.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return;
    };
    if inner.trim().is_empty() {
        return;
    }
    if !*first {
        out.push(',');
    }
    *first = false;
    out.push_str(inner);
}

fn rich_nodes_json(elems: &[Element]) -> String {
    let mut out = String::from("[");
    let mut first = true;
    let skip = display_skip_indices(elems);
    let mut consumed = HashSet::new();
    for (idx, e) in elems.iter().enumerate() {
        if skip.contains(&idx) || consumed.contains(&idx) {
            continue;
        }
        if is_system_decoration_element(e) {
            continue;
        }
        let mut node = String::new();
        let video_candidates = video_candidate_texts(e);
        if is_mmt_transport_index(e) {
            let children = mmt_display_elements(elems, idx);
            if !children.is_empty() {
                for next in idx + 1..elems.len() {
                    consumed.insert(next);
                }
                let child_nodes = rich_nodes_json(&children);
                if first_txdata_named_uint(e, &["dwPacketCount"]).unwrap_or(0) <= 1 {
                    append_json_array_items(&mut out, &mut first, &child_nodes);
                    continue;
                } else {
                    node = format!(
                        "{{\"type\":\"nested\",\"text\":\"{}\",\"children\":{},\"raw_text\":\"{}\"}}",
                        json_escape("[聊天记录]"),
                        child_nodes,
                        json_escape(&plain_text(&children))
                    );
                }
            }
        } else {
            let children = embedded_payload_elements(e);
            if !children.is_empty() {
                node = format!(
                    "{{\"type\":\"nested\",\"text\":\"{}\",\"children\":{},\"raw_text\":\"{}\"}}",
                    json_escape("[内嵌消息]"),
                    rich_nodes_json(&children),
                    json_escape(&plain_text(&children))
                );
            }
        }
        if node.is_empty() {
            if !video_candidates.is_empty() {
                node = format!("{{\"type\":\"video\",\"text\":\"[视频]\",\"candidates\":[");
                for (i, text) in video_candidates.iter().enumerate() {
                    if i != 0 {
                        node.push(',');
                    }
                    node.push_str(&format!("\"{}\"", json_escape(text)));
                }
                node.push_str("]}");
            } else {
                match e.typ {
                    1 => {
                        let links = link_texts(e);
                        if !links.is_empty() {
                            let text = if e.summary.trim().is_empty() {
                                links[0].as_str()
                            } else {
                                e.summary.trim()
                            };
                            node = rich_link_node_json(text, &links);
                        } else {
                            let candidates = element_candidate_texts(e);
                            if !candidates.is_empty()
                                && [
                                    "打开文件",
                                    "打开所在文件夹",
                                    "继续发送",
                                    "到云端查看",
                                    "撤回该文件",
                                ]
                                .iter()
                                .any(|needle| e.summary.contains(needle))
                            {
                                let kind = media_node_kind(&candidates);
                                node = format!(
                                    "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                    kind,
                                    json_escape(e.summary.trim())
                                );
                                for (i, text) in candidates.iter().enumerate() {
                                    if i != 0 {
                                        node.push(',');
                                    }
                                    node.push_str(&format!("\"{}\"", json_escape(text)));
                                }
                                node.push_str("]}");
                            } else if !candidates.is_empty()
                                && (e.summary.trim().is_empty()
                                    || has_txdata_named_field(
                                        e,
                                        &[
                                            "strPicInfoFileName",
                                            "bufPicInfoServerPath",
                                            "bufPicResId",
                                        ],
                                    ))
                            {
                                let kind = media_node_kind(&candidates);
                                let label = match kind {
                                    "image" => "[图片]",
                                    "video" => "[视频]",
                                    "voice" => "[语音]",
                                    _ => "[文件]",
                                };
                                node = format!(
                                    "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                    kind, label
                                );
                                for (i, text) in candidates.iter().enumerate() {
                                    if i != 0 {
                                        node.push(',');
                                    }
                                    node.push_str(&format!("\"{}\"", json_escape(text)));
                                }
                                node.push_str("]}");
                            } else {
                                let text =
                                    mention_with_inline_suffix(elems, idx, &skip, &mut consumed);
                                if !text.is_empty() {
                                    let node_type = if is_reply_mention_text(e) {
                                        "mention"
                                    } else {
                                        "text"
                                    };
                                    node = format!(
                                        "{{\"type\":\"{}\",\"text\":\"{}\"}}",
                                        node_type,
                                        json_escape(&text)
                                    );
                                }
                            }
                        }
                    }
                    0 => {
                        let candidates = element_candidate_texts(e);
                        if !candidates.is_empty() {
                            let kind = media_node_kind(&candidates);
                            let label = match kind {
                                "image" => "[图片]",
                                "video" => "[视频]",
                                "voice" => "[语音]",
                                _ => "[文件]",
                            };
                            node = format!(
                                "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                kind, label
                            );
                            for (i, text) in candidates.iter().enumerate() {
                                if i != 0 {
                                    node.push(',');
                                }
                                node.push_str(&format!("\"{}\"", json_escape(text)));
                            }
                            node.push_str("]}");
                        } else if !e.summary.is_empty() {
                            node = format!(
                                "{{\"type\":\"text\",\"text\":\"{}\"}}",
                                json_escape(&e.summary)
                            );
                        }
                    }
                    2 => {
                        let links = link_texts(e);
                        if !links.is_empty() {
                            let text = if e.summary.trim().is_empty() {
                                links[0].as_str()
                            } else {
                                e.summary.trim()
                            };
                            node = rich_link_node_json(text, &links);
                        } else if !e.summary.is_empty() {
                            node = format!(
                                "{{\"type\":\"face\",\"text\":\"{}\"",
                                json_escape(&e.summary)
                            );
                            if let Some(id) = sysface_id(e) {
                                node.push_str(&format!(
                                    ",\"face_id\":{},\"candidates\":[\"SysFaceResFileSystem:{}.gif\",\"SysFaceResFileSystem:apng\\\\{}.png\"]",
                                    id, id, id
                                ));
                            }
                            node.push('}');
                        }
                    }
                    0x0d => {
                        if !e.summary.is_empty() {
                            node = format!(
                                "{{\"type\":\"emoji\",\"text\":\"{}\"}}",
                                json_escape(&e.summary)
                            );
                        }
                    }
                    0x0c => {
                        if !e.summary.is_empty() {
                            let typ = if is_qq_face_label(&e.summary) {
                                "face"
                            } else {
                                "text"
                            };
                            node = format!(
                                "{{\"type\":\"{}\",\"text\":\"{}\"}}",
                                typ,
                                json_escape(&e.summary)
                            );
                        }
                    }
                    3 | 5 | 6 => {
                        let candidates = element_candidate_texts(e);
                        if !candidates.is_empty() {
                            let kind = media_node_kind(&candidates);
                            let label = match kind {
                                "image" => "[图片]",
                                "video" => "[视频]",
                                "voice" => "[语音]",
                                _ => "[文件]",
                            };
                            node = format!(
                                "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                kind,
                                json_escape(if e.summary.is_empty() {
                                    label
                                } else {
                                    &e.summary
                                })
                            );
                            for (i, text) in candidates.iter().enumerate() {
                                if i != 0 {
                                    node.push(',');
                                }
                                node.push_str(&format!("\"{}\"", json_escape(text)));
                            }
                            node.push_str("]}");
                        } else if !e.summary.is_empty() {
                            let typ = if e.summary.starts_with("SmallEmoji:") {
                                "emoji"
                            } else {
                                "text"
                            };
                            node = format!(
                                "{{\"type\":\"{}\",\"text\":\"{}\"}}",
                                typ,
                                json_escape(&e.summary)
                            );
                        }
                    }
                    7 | 0x11 => {
                        let candidates = element_candidate_texts(e);
                        if !candidates.is_empty() {
                            let kind = media_node_kind(&candidates);
                            let label = match kind {
                                "image" => "[图片]",
                                "video" => "[视频]",
                                "voice" => "[语音]",
                                _ => "[文件]",
                            };
                            node = format!(
                                "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                kind,
                                json_escape(if e.summary.is_empty() {
                                    label
                                } else {
                                    &e.summary
                                })
                            );
                            for (i, text) in candidates.iter().enumerate() {
                                if i != 0 {
                                    node.push(',');
                                }
                                node.push_str(&format!("\"{}\"", json_escape(text)));
                            }
                            node.push_str("]");
                            if kind == "file" {
                                node.push_str(&format!(
                                    ",\"file_meta\":{}",
                                    file_transfer_meta_json(e)
                                ));
                            }
                            node.push('}');
                        } else if !e.summary.is_empty() && e.summary != "[文件]" {
                            node = format!(
                                "{{\"type\":\"text\",\"text\":\"{}\"}}",
                                json_escape(&e.summary)
                            );
                        }
                    }
                    0x10 | 0x15 | 0x16 => {
                        let mut candidates = element_candidate_texts(e);
                        if is_stable_media_candidate(&e.summary) {
                            push_unique_text(&mut candidates, e.summary.trim());
                            if !e.summary.starts_with("UserData") && !e.summary.contains(':') {
                                push_unique_text(
                                    &mut candidates,
                                    &format!("UserDataFileRecv:{}", e.summary.trim()),
                                );
                            }
                        }
                        if !candidates.is_empty() {
                            let kind = media_node_kind(&candidates);
                            node = format!(
                                "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                kind,
                                json_escape(if e.summary.is_empty() {
                                    "[文件]"
                                } else {
                                    &e.summary
                                })
                            );
                            for (i, text) in candidates.iter().enumerate() {
                                if i != 0 {
                                    node.push(',');
                                }
                                node.push_str(&format!("\"{}\"", json_escape(text)));
                            }
                            node.push_str("]}");
                        }
                    }
                    0x18 => {
                        let candidates = file_transfer_candidates(e);
                        if !candidates.is_empty() {
                            let kind = media_node_kind(&candidates);
                            let label = match kind {
                                "image" => "[图片]",
                                "video" => "[视频]",
                                "voice" => "[语音]",
                                _ => "[文件]",
                            };
                            node = format!(
                                "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                kind,
                                json_escape(if e.summary.is_empty() {
                                    label
                                } else {
                                    &e.summary
                                })
                            );
                            for (i, text) in candidates.iter().enumerate() {
                                if i != 0 {
                                    node.push(',');
                                }
                                node.push_str(&format!("\"{}\"", json_escape(text)));
                            }
                            node.push_str("]");
                            if kind == "file" {
                                node.push_str(&format!(
                                    ",\"file_meta\":{}",
                                    file_transfer_meta_json(e)
                                ));
                            }
                            node.push('}');
                        } else if !e.summary.is_empty() && e.summary != "[文件]" {
                            node = format!(
                                "{{\"type\":\"text\",\"text\":\"{}\"}}",
                                json_escape(&e.summary)
                            );
                        }
                    }
                    0x14 => {
                        let xml = {
                            let text = struct_msg_xml(e);
                            if text.is_empty() {
                                field_text(e, 1)
                            } else {
                                text
                            }
                        };
                        let abstract_text = field_text(e, 2);
                        let href = extract_xml_attr(&xml, "url");
                        let brief = extract_xml_attr(&xml, "brief");
                        let text = if !abstract_text.is_empty() {
                            abstract_text
                        } else if !brief.is_empty() {
                            brief
                        } else {
                            struct_msg_display_text(&xml, &e.summary)
                        };
                        let candidates = element_candidate_texts(e);
                        if xml.is_empty() && href.is_empty() && !candidates.is_empty() {
                            let kind = media_node_kind(&candidates);
                            let label = match kind {
                                "image" => "[图片]",
                                "video" => "[视频]",
                                "voice" => "[语音]",
                                _ => "[文件]",
                            };
                            node = format!(
                                "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                kind,
                                json_escape(label)
                            );
                            for (i, text) in candidates.iter().enumerate() {
                                if i != 0 {
                                    node.push(',');
                                }
                                node.push_str(&format!("\"{}\"", json_escape(text)));
                            }
                            node.push_str("]}");
                        } else if !href.is_empty() || !xml.is_empty() {
                            node = format!(
                            "{{\"type\":\"rich\",\"text\":\"{}\",\"href\":\"{}\",\"xml\":\"{}\"}}",
                            json_escape(&text),
                            json_escape(&href),
                            json_escape(&xml)
                        );
                        } else if !text.is_empty() {
                            node = format!(
                                "{{\"type\":\"text\",\"text\":\"{}\"}}",
                                json_escape(&text)
                            );
                        }
                    }
                    0x19 => {
                        if is_reply_abstract(e) {
                            let text = reply_quote_text(e);
                            if !text.is_empty() {
                                let href = reply_quote_href(e);
                                if href.is_empty() {
                                    node = format!(
                                        "{{\"type\":\"quote\",\"text\":\"{}\"}}",
                                        json_escape(&text)
                                    );
                                } else {
                                    node = format!(
                                        "{{\"type\":\"quote\",\"text\":\"{}\",\"href\":\"{}\"}}",
                                        json_escape(&text),
                                        json_escape(&href)
                                    );
                                }
                            }
                        } else if is_common_elem_face(e) {
                            node = format!(
                                "{{\"type\":\"face\",\"text\":\"{}\"}}",
                                json_escape(e.summary.trim())
                            );
                        } else {
                            let res_id = new_sys_face_resource_id(e);
                            if !res_id.is_empty() {
                                node = format!(
                            "{{\"type\":\"face\",\"text\":\"[新系统表情]\",\"resource_id\":\"{}\"}}",
                            json_escape(&res_id)
                        );
                            } else {
                                let candidates = element_candidate_texts(e);
                                if !candidates.is_empty() {
                                    let kind = media_node_kind(&candidates);
                                    let label = match kind {
                                        "image" => "[图片]",
                                        "video" => "[视频]",
                                        "voice" => "[语音]",
                                        _ => "[文件]",
                                    };
                                    node = format!(
                                        "{{\"type\":\"{}\",\"text\":\"{}\",\"candidates\":[",
                                        kind, label
                                    );
                                    for (i, text) in candidates.iter().enumerate() {
                                        if i != 0 {
                                            node.push(',');
                                        }
                                        node.push_str(&format!("\"{}\"", json_escape(text)));
                                    }
                                    node.push_str("]}");
                                }
                            }
                        }
                    }
                    0x1b => {
                        let text = ark_app_display_text(e);
                        let href = ark_app_href(e);
                        if !href.is_empty() {
                            node = format!(
                                "{{\"type\":\"rich\",\"text\":\"{}\",\"href\":\"{}\"}}",
                                json_escape(&text),
                                json_escape(&href)
                            );
                        } else if !text.is_empty() {
                            node =
                                format!("{{\"type\":\"app\",\"text\":\"{}\"}}", json_escape(&text));
                        }
                    }
                    0x1e => {
                        let text = multi_msg_display_text(e);
                        let previews = title_list_preview(e, 6);
                        if !text.is_empty() || !previews.is_empty() {
                            let res_id = first_multi_msg_named_text(e, "bsResId");
                            let file_name = first_multi_msg_named_text(e, "bsFileName");
                            let item_count = first_txdata_named_uint(e, &["dwMsgPackListSize"])
                                .unwrap_or(previews.len() as u64);
                            let expand_status = if res_id.is_empty() && file_name.is_empty() {
                                "inline_preview_only"
                            } else {
                                "external_mmp_required"
                            };
                            let expand_reason = if expand_status == "external_mmp_required" {
                                "当前MsgContent只包含titleList预览；完整展开需要bsResId/bsFileName对应的MMP数据流"
                            } else {
                                "当前MsgContent只包含聊天记录预览"
                            };
                            node = format!(
                                "{{\"type\":\"multi_msg\",\"text\":\"{}\",\"res_id\":\"{}\",\"file_name\":\"{}\",\"item_count\":{},\"preview_count\":{},\"expand_status\":\"{}\",\"expand_reason\":\"{}\",\"items\":[",
                                json_escape(if text.is_empty() {
                                    "[聊天记录]"
                                } else {
                                    &text
                                }),
                                json_escape(&res_id),
                                json_escape(&file_name),
                                item_count,
                                previews.len(),
                                expand_status,
                                json_escape(expand_reason)
                            );
                            for (i, item) in previews.iter().enumerate() {
                                if i != 0 {
                                    node.push(',');
                                }
                                node.push_str(&format!(
                                    "{{\"type\":\"multi_item\",\"text\":\"{}\"}}",
                                    json_escape(item)
                                ));
                            }
                            node.push_str("]}");
                        }
                    }
                    _ => {}
                }
            }
        }
        if !node.is_empty() {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&node);
        }
    }
    if first {
        let label = transport_only_label(elems);
        if !label.is_empty() {
            out.push_str(&format!(
                "{{\"type\":\"system\",\"text\":\"{}\"}}",
                json_escape(&label)
            ));
        }
    }
    out.push(']');
    out
}

fn rich_html_fragment(elems: &[Element]) -> String {
    let mut out = String::new();
    let skip = display_skip_indices(elems);
    let mut consumed = HashSet::new();
    for (idx, e) in elems.iter().enumerate() {
        if skip.contains(&idx) || consumed.contains(&idx) {
            continue;
        }
        if is_system_decoration_element(e) {
            continue;
        }
        if is_mmt_transport_index(e) {
            let children = mmt_display_elements(elems, idx);
            if !children.is_empty() {
                for next in idx + 1..elems.len() {
                    consumed.insert(next);
                }
                let child_html = rich_html_fragment(&children);
                if first_txdata_named_uint(e, &["dwPacketCount"]).unwrap_or(0) <= 1 {
                    out.push_str(&child_html);
                } else {
                    out.push_str("<qq-nested>");
                    out.push_str(&child_html);
                    out.push_str("</qq-nested>");
                }
            }
            continue;
        }
        let children = embedded_payload_elements(e);
        if !children.is_empty() {
            out.push_str("<qq-nested>");
            out.push_str(&rich_html_fragment(&children));
            out.push_str("</qq-nested>");
            continue;
        }
        match e.typ {
            1 => {
                let links = link_texts(e);
                if !links.is_empty() {
                    let text = if e.summary.trim().is_empty() {
                        links[0].as_str()
                    } else {
                        e.summary.trim()
                    };
                    out.push_str(&format!(
                        "<qq-link href=\"{}\">{}</qq-link>",
                        html_escape(&links[0]),
                        html_escape(text)
                    ));
                } else {
                    let candidates = element_candidate_texts(e);
                    if !candidates.is_empty()
                        && [
                            "打开文件",
                            "打开所在文件夹",
                            "继续发送",
                            "到云端查看",
                            "撤回该文件",
                        ]
                        .iter()
                        .any(|needle| e.summary.contains(needle))
                    {
                        let attr = candidates
                            .first()
                            .map(|text| format!(" path=\"{}\"", html_escape(text)))
                            .unwrap_or_default();
                        match media_node_kind(&candidates) {
                            "image" => out.push_str(&format!(
                                "<qq-image{}>{}</qq-image>",
                                attr,
                                html_escape(e.summary.trim())
                            )),
                            "video" => out.push_str(&format!(
                                "<qq-video{}>{}</qq-video>",
                                attr,
                                html_escape(e.summary.trim())
                            )),
                            "voice" => out.push_str(&format!(
                                "<qq-voice{}>{}</qq-voice>",
                                attr,
                                html_escape(e.summary.trim())
                            )),
                            _ => out.push_str(&format!(
                                "<qq-file{}>{}</qq-file>",
                                attr,
                                html_escape(e.summary.trim())
                            )),
                        }
                    } else if !candidates.is_empty()
                        && (e.summary.trim().is_empty()
                            || has_txdata_named_field(
                                e,
                                &["strPicInfoFileName", "bufPicInfoServerPath", "bufPicResId"],
                            ))
                    {
                        let kind = media_node_kind(&candidates);
                        let attr = candidates
                            .first()
                            .map(|text| format!(" path=\"{}\"", html_escape(text)))
                            .unwrap_or_default();
                        match kind {
                            "image" => {
                                out.push_str(&format!("<qq-image{}>[图片]</qq-image>", attr))
                            }
                            "video" => {
                                out.push_str(&format!("<qq-video{}>[视频]</qq-video>", attr))
                            }
                            "voice" => {
                                out.push_str(&format!("<qq-voice{}>[语音]</qq-voice>", attr))
                            }
                            _ => out.push_str(&format!("<qq-file{}>[文件]</qq-file>", attr)),
                        }
                    } else {
                        let text = mention_with_inline_suffix(elems, idx, &skip, &mut consumed);
                        out.push_str(&html_escape(&text));
                    }
                }
            }
            0 => {
                let candidates = element_candidate_texts(e);
                if !candidates.is_empty() {
                    let kind = media_node_kind(&candidates);
                    let attr = candidates
                        .first()
                        .map(|text| format!(" path=\"{}\"", html_escape(text)))
                        .unwrap_or_default();
                    match kind {
                        "image" => out.push_str(&format!("<qq-image{}>[图片]</qq-image>", attr)),
                        "video" => out.push_str(&format!("<qq-video{}>[视频]</qq-video>", attr)),
                        "voice" => out.push_str(&format!("<qq-voice{}>[语音]</qq-voice>", attr)),
                        _ => out.push_str(&format!("<qq-file{}>[文件]</qq-file>", attr)),
                    }
                } else if !e.summary.is_empty() {
                    out.push_str(&html_escape(&e.summary));
                }
            }
            2 => {
                let links = link_texts(e);
                if !links.is_empty() {
                    let text = if e.summary.trim().is_empty() {
                        links[0].as_str()
                    } else {
                        e.summary.trim()
                    };
                    out.push_str(&format!(
                        "<qq-link href=\"{}\">{}</qq-link>",
                        html_escape(&links[0]),
                        html_escape(text)
                    ));
                } else if !e.summary.is_empty() {
                    out.push_str(&format!("<qq-face>{}</qq-face>", html_escape(&e.summary)));
                }
            }
            0x0d => {
                if !e.summary.is_empty() {
                    out.push_str(&format!("<qq-emoji>{}</qq-emoji>", html_escape(&e.summary)));
                }
            }
            0x0c => {
                if !e.summary.is_empty() {
                    out.push_str(&html_escape(&e.summary));
                }
            }
            3 | 5 | 6 => {
                let candidates = element_candidate_texts(e);
                if !candidates.is_empty() {
                    let kind = media_node_kind(&candidates);
                    let attr = candidates
                        .first()
                        .map(|text| format!(" path=\"{}\"", html_escape(text)))
                        .unwrap_or_default();
                    match kind {
                        "image" => out.push_str(&format!(
                            "<qq-image{}>{}</qq-image>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[图片]"
                            } else {
                                &e.summary
                            })
                        )),
                        "video" => out.push_str(&format!(
                            "<qq-video{}>{}</qq-video>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[视频]"
                            } else {
                                &e.summary
                            })
                        )),
                        "voice" => out.push_str(&format!(
                            "<qq-voice{}>{}</qq-voice>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[语音]"
                            } else {
                                &e.summary
                            })
                        )),
                        _ => out.push_str(&format!(
                            "<qq-file{}>{}</qq-file>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[文件]"
                            } else {
                                &e.summary
                            })
                        )),
                    }
                } else if !e.summary.is_empty() {
                    if e.summary.starts_with("SmallEmoji:") {
                        out.push_str(&format!("<qq-emoji>{}</qq-emoji>", html_escape(&e.summary)));
                    } else {
                        out.push_str(&html_escape(&e.summary));
                    }
                }
            }
            7 | 0x11 => {
                let candidates = element_candidate_texts(e);
                if !candidates.is_empty() {
                    let kind = media_node_kind(&candidates);
                    let attr = candidates
                        .first()
                        .map(|text| format!(" path=\"{}\"", html_escape(text)))
                        .unwrap_or_default();
                    match kind {
                        "image" => out.push_str(&format!(
                            "<qq-image{}>{}</qq-image>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[图片]"
                            } else {
                                &e.summary
                            })
                        )),
                        "video" => out.push_str(&format!(
                            "<qq-video{}>{}</qq-video>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[视频]"
                            } else {
                                &e.summary
                            })
                        )),
                        "voice" => out.push_str(&format!(
                            "<qq-voice{}>{}</qq-voice>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[语音]"
                            } else {
                                &e.summary
                            })
                        )),
                        _ => out.push_str(&format!(
                            "<qq-file{}>{}</qq-file>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[文件]"
                            } else {
                                &e.summary
                            })
                        )),
                    }
                } else if !e.summary.is_empty() && e.summary != "[文件]" {
                    out.push_str(&html_escape(&e.summary));
                }
            }
            0x10 | 0x15 | 0x16 => {
                let mut candidates = element_candidate_texts(e);
                if is_stable_media_candidate(&e.summary) {
                    push_unique_text(&mut candidates, e.summary.trim());
                    if !e.summary.starts_with("UserData") && !e.summary.contains(':') {
                        push_unique_text(
                            &mut candidates,
                            &format!("UserDataFileRecv:{}", e.summary.trim()),
                        );
                    }
                }
                if !candidates.is_empty() {
                    let kind = media_node_kind(&candidates);
                    let attr = candidates
                        .first()
                        .map(|text| format!(" path=\"{}\"", html_escape(text)))
                        .unwrap_or_default();
                    match kind {
                        "image" => out.push_str(&format!(
                            "<qq-image{}>{}</qq-image>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[图片]"
                            } else {
                                &e.summary
                            })
                        )),
                        "video" => out.push_str(&format!(
                            "<qq-video{}>{}</qq-video>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[视频]"
                            } else {
                                &e.summary
                            })
                        )),
                        "voice" => out.push_str(&format!(
                            "<qq-voice{}>{}</qq-voice>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[语音]"
                            } else {
                                &e.summary
                            })
                        )),
                        _ => out.push_str(&format!(
                            "<qq-file{}>{}</qq-file>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[文件]"
                            } else {
                                &e.summary
                            })
                        )),
                    }
                }
            }
            0x18 => {
                let candidates = file_transfer_candidates(e);
                if !candidates.is_empty() {
                    let kind = media_node_kind(&candidates);
                    let attr = candidates
                        .first()
                        .map(|text| format!(" path=\"{}\"", html_escape(text)))
                        .unwrap_or_default();
                    match kind {
                        "image" => out.push_str(&format!(
                            "<qq-image{}>{}</qq-image>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[图片]"
                            } else {
                                &e.summary
                            })
                        )),
                        "video" => out.push_str(&format!(
                            "<qq-video{}>{}</qq-video>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[视频]"
                            } else {
                                &e.summary
                            })
                        )),
                        "voice" => out.push_str(&format!(
                            "<qq-voice{}>{}</qq-voice>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[语音]"
                            } else {
                                &e.summary
                            })
                        )),
                        _ => out.push_str(&format!(
                            "<qq-file{}>{}</qq-file>",
                            attr,
                            html_escape(if e.summary.is_empty() {
                                "[文件]"
                            } else {
                                &e.summary
                            })
                        )),
                    }
                } else if !e.summary.is_empty() && e.summary != "[文件]" {
                    out.push_str(&html_escape(&e.summary));
                }
            }
            0x1a => {
                let candidates = video_candidate_texts(e);
                let attr = candidates
                    .first()
                    .map(|text| format!(" path=\"{}\"", html_escape(text)))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "<qq-video{}>{}</qq-video>",
                    attr,
                    html_escape(if e.summary.is_empty() {
                        "[视频]"
                    } else {
                        &e.summary
                    })
                ));
            }
            0x14 => {
                let xml = {
                    let text = struct_msg_xml(e);
                    if text.is_empty() {
                        field_text(e, 1)
                    } else {
                        text
                    }
                };
                let href = extract_xml_attr(&xml, "url");
                let abstract_text = field_text(e, 2);
                let text = if !abstract_text.is_empty() {
                    abstract_text
                } else {
                    struct_msg_display_text(&xml, &e.summary)
                };
                let candidates = element_candidate_texts(e);
                if xml.is_empty() && href.is_empty() && !candidates.is_empty() {
                    let kind = media_node_kind(&candidates);
                    let attr = candidates
                        .first()
                        .map(|text| format!(" path=\"{}\"", html_escape(text)))
                        .unwrap_or_default();
                    match kind {
                        "image" => out.push_str(&format!("<qq-image{}>[图片]</qq-image>", attr)),
                        "video" => out.push_str(&format!("<qq-video{}>[视频]</qq-video>", attr)),
                        "voice" => out.push_str(&format!("<qq-voice{}>[语音]</qq-voice>", attr)),
                        _ => out.push_str(&format!("<qq-file{}>[文件]</qq-file>", attr)),
                    }
                } else if !href.is_empty() {
                    out.push_str(&format!(
                        "<qq-link href=\"{}\">{}</qq-link>",
                        html_escape(&href),
                        html_escape(&text)
                    ));
                } else if !text.is_empty() {
                    out.push_str(&html_escape(&text));
                }
            }
            0x19 => {
                if is_reply_abstract(e) {
                    let text = reply_quote_text(e);
                    if !text.is_empty() {
                        let href = reply_quote_href(e);
                        if href.is_empty() {
                            out.push_str(&format!("<qq-quote>{}</qq-quote>", html_escape(&text)));
                        } else {
                            out.push_str(&format!(
                                "<qq-quote href=\"{}\">{}</qq-quote>",
                                html_escape(&href),
                                html_escape(&text)
                            ));
                        }
                    }
                } else if is_common_elem_face(e) {
                    out.push_str(&format!(
                        "<qq-face>{}</qq-face>",
                        html_escape(e.summary.trim())
                    ));
                } else {
                    let res_id = new_sys_face_resource_id(e);
                    if !res_id.is_empty() {
                        out.push_str(&format!(
                            "<qq-face data-res-id=\"{}\">[新系统表情]</qq-face>",
                            html_escape(&res_id)
                        ));
                    } else {
                        let candidates = element_candidate_texts(e);
                        if !candidates.is_empty() {
                            let kind = media_node_kind(&candidates);
                            let attr = candidates
                                .first()
                                .map(|text| format!(" path=\"{}\"", html_escape(text)))
                                .unwrap_or_default();
                            match kind {
                                "image" => {
                                    out.push_str(&format!("<qq-image{}>[图片]</qq-image>", attr))
                                }
                                "video" => {
                                    out.push_str(&format!("<qq-video{}>[视频]</qq-video>", attr))
                                }
                                "voice" => {
                                    out.push_str(&format!("<qq-voice{}>[语音]</qq-voice>", attr))
                                }
                                _ => out.push_str(&format!("<qq-file{}>[文件]</qq-file>", attr)),
                            }
                        }
                    }
                }
            }
            0x1b => {
                let text = ark_app_display_text(e);
                let href = ark_app_href(e);
                if !href.is_empty() {
                    out.push_str(&format!(
                        "<qq-link href=\"{}\">{}</qq-link>",
                        html_escape(&href),
                        html_escape(&text)
                    ));
                } else if !text.is_empty() {
                    out.push_str(&html_escape(&text));
                }
            }
            0x1e => {
                let text = multi_msg_display_text(e);
                let previews = title_list_preview(e, 6);
                if !text.is_empty() || !previews.is_empty() {
                    out.push_str(&format!(
                        "<qq-multi-msg>{}",
                        html_escape(if text.is_empty() {
                            "[聊天记录]"
                        } else {
                            &text
                        })
                    ));
                    for item in previews {
                        out.push_str(&format!(
                            "<qq-multi-item>{}</qq-multi-item>",
                            html_escape(&item)
                        ));
                    }
                    out.push_str("</qq-multi-msg>");
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        let label = transport_only_label(elems);
        if !label.is_empty() {
            out.push_str(&format!("<qq-system>{}</qq-system>", html_escape(&label)));
        }
    }
    out
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    let clean: Vec<u8> = s.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < clean.len() {
        let hi = (clean[i] as char).to_digit(16).unwrap_or(0);
        let lo = (clean[i + 1] as char).to_digit(16).unwrap_or(0);
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    out
}

pub fn parse_msgcontent_outputs(data: &[u8]) -> (String, String, String, String) {
    let elems = parse_msgcontent(data);
    (
        plain_text(&elems),
        elements_json(&elems),
        rich_nodes_json(&elems),
        rich_html_fragment(&elems),
    )
}

fn split_mmp_msgpack_stream(data: &[u8]) -> Vec<&[u8]> {
    if data.len() < 8 {
        return Vec::new();
    }
    let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let total = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    if count == 0 || count > 200 || total != data.len().saturating_sub(8) {
        return Vec::new();
    }
    let mut pos = 8usize;
    let mut out = Vec::new();
    for _ in 0..count {
        if pos + 4 > data.len() {
            return Vec::new();
        }
        let len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if len < 8 || pos + len > data.len() || !data[pos..].starts_with(b"MSG") {
            return Vec::new();
        }
        out.push(&data[pos..pos + len]);
        pos += len;
    }
    if pos == data.len() {
        out
    } else {
        Vec::new()
    }
}

fn msg_header_time_rand(data: &[u8]) -> (Option<u32>, Option<u32>) {
    if data.len() < 16 || !data.starts_with(b"MSG") {
        return (None, None);
    }
    let time = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    let rand = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    (Some(time), Some(rand))
}

fn mmp_item_meta(elems: &[Element]) -> (Option<u64>, String, Option<u64>) {
    for e in elems {
        let sender_uin = first_txdata_named_uint(e, &["dwSenderUin"]);
        let sender_name = first_txdata_named_text(e, &["strSenderName", "strSenderShowName"]);
        let msg_seq = first_txdata_named_uint(e, &["dwMsgSeq"]);
        if sender_uin.is_some() || !sender_name.trim().is_empty() || msg_seq.is_some() {
            return (sender_uin, sender_name, msg_seq);
        }
    }
    (None, String::new(), None)
}

pub fn parse_info_mmp_items_json(data: &[u8]) -> String {
    let mut records = Vec::new();
    parse_td_ta_records_at(data, 0, 0, &mut records);
    for rec in records {
        for field in rec.txdata_fields {
            if field.name != "buffMsgPackListStream" {
                continue;
            }
            let plain = txdata_lenkey_plain(&field.value);
            let items = split_mmp_msgpack_stream(&plain);
            if items.is_empty() {
                return "[]".to_string();
            }
            let mut out = String::from("[");
            for (idx, item) in items.iter().enumerate() {
                if idx != 0 {
                    out.push(',');
                }
                let elems = parse_msgcontent(item);
                let text = plain_text(&elems);
                let elements = elements_json(&elems);
                let rich_nodes = rich_nodes_json(&elems);
                let rich_html = rich_html_fragment(&elems);
                let (time, rand) = msg_header_time_rand(item);
                let (sender_uin, sender_name, msg_seq) = mmp_item_meta(&elems);
                let time_json = time
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".to_string());
                let rand_json = rand
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".to_string());
                let sender_uin_json = sender_uin
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".to_string());
                let msg_seq_json = msg_seq
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".to_string());
                out.push_str(&format!(
                    "{{\"type\":\"mmp_item\",\"index\":{},\"msg_len\":{},\"time\":{},\"rand\":{},\"sender_uin\":{},\"sender_name\":\"{}\",\"msg_seq\":{},\"text\":\"{}\",\"elements\":{},\"rich_nodes\":{},\"rich_html\":\"{}\"}}",
                    idx,
                    item.len(),
                    time_json,
                    rand_json,
                    sender_uin_json,
                    json_escape(&sender_name),
                    msg_seq_json,
                    json_escape(&text),
                    elements,
                    rich_nodes,
                    json_escape(&rich_html)
                ));
            }
            out.push(']');
            return out;
        }
    }
    "[]".to_string()
}

fn txfield_json_object(field: &TxField) -> String {
    let mut parts = vec![
        format!("\"header\":{}", field.header),
        format!("\"name\":\"{}\"", json_escape(&field.name)),
        format!("\"value_len\":{}", field.value_len),
        format!("\"value_hex\":\"{}\"", hex_bytes(&field.value, 4096)),
    ];
    if field.value.len() == 4 {
        let value = u32::from_le_bytes([
            field.value[0],
            field.value[1],
            field.value[2],
            field.value[3],
        ]);
        parts.push(format!("\"value_uint\":{}", value));
    } else if field.value.len() == 8 {
        let value = u64::from_le_bytes([
            field.value[0],
            field.value[1],
            field.value[2],
            field.value[3],
            field.value[4],
            field.value[5],
            field.value[6],
            field.value[7],
        ]);
        parts.push(format!("\"value_uint\":{}", value));
    }
    if !field.value_texts.is_empty() {
        parts.push(format!(
            "\"value_texts\":[{}]",
            field
                .value_texts
                .iter()
                .map(|s| format!("\"{}\"", json_escape(s)))
                .collect::<Vec<_>>()
                .join(",")
        ));
        if field.value_text_len > 0 && field.value_text_len <= field.value.len() {
            parts.push(format!("\"value_text_len\":{}", field.value_text_len));
            if field.value_text_len < field.value.len() {
                parts.push(format!(
                    "\"value_unparsed_hex\":\"{}\"",
                    hex_bytes(&field.value[field.value_text_len..], 96)
                ));
            }
        }
    }
    format!("{{{}}}", parts.join(","))
}

pub fn parse_info_json(data: &[u8]) -> String {
    let records = parse_td_records(data);
    let mut out = Vec::new();
    for rec in records {
        if rec.txdata_fields.is_empty() {
            continue;
        }
        out.push(format!(
            "{{\"offset\":{},\"tag\":\"{}\",\"txdata_field_count\":{},\"txdata_complete\":{},\"txdata_fields\":[{}]}}",
            rec.offset,
            json_escape(&rec.tag),
            rec.txdata_field_count,
            if rec.txdata_complete { "true" } else { "false" },
            rec.txdata_fields
                .iter()
                .map(txfield_json_object)
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    format!("{{\"td_ta_records\":[{}]}}", out.join(","))
}

fn usage() {
    eprintln!("usage: msg3_richtext_parser_rs --input rows.tsv --out out.csv");
}

pub fn cli_main() -> std::io::Result<()> {
    let mut input = String::new();
    let mut out_path = String::new();
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--input" if i + 1 < args.len() => {
                input = args[i + 1].clone();
                i += 2;
            }
            "--out" if i + 1 < args.len() => {
                out_path = args[i + 1].clone();
                i += 2;
            }
            "--audit" => i += 1,
            _ => {
                usage();
                std::process::exit(2);
            }
        }
    }
    if input.is_empty() || out_path.is_empty() {
        usage();
        std::process::exit(2);
    }

    let reader = BufReader::new(File::open(input)?);
    let mut out = BufWriter::new(File::create(out_path)?);
    out.write_all(b"\xEF\xBB\xBFtable,rowid,time,rand,sender_uin,plain_text,elements_json,rich_nodes_json,rich_html\n")?;
    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        if line_no == 0 && line.starts_with("table\t") {
            continue;
        }
        let parts: Vec<&str> = line.splitn(6, '\t').collect();
        if parts.len() != 6 {
            continue;
        }
        let table = parts[0];
        let rowid = parts[1];
        let time = parts[2];
        let rand = parts[3];
        let sender = parts[4];
        let data = hex_to_bytes(parts[5]);
        let elems = parse_msgcontent(&data);
        let text = plain_text(&elems);
        let json = elements_json(&elems);
        let rich_nodes = rich_nodes_json(&elems);
        let rich_html = rich_html_fragment(&elems);
        writeln!(
            out,
            "{},{},{},{},{},{},{},{},{}",
            csv_escape(table),
            rowid,
            time,
            rand,
            sender,
            csv_escape(&text),
            csv_escape(&json),
            csv_escape(&rich_nodes),
            csv_escape(&rich_html)
        )?;
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct TxDataField {
    pub header: u8,
    pub name: String,
    pub value_len: u32,
    pub value: Vec<u8>,
    pub value_texts: Vec<String>,
    pub value_text_len: usize,
}

#[derive(Clone, Debug, Default)]
pub struct TxDataRecord {
    pub count: u16,
    pub complete: bool,
    pub fields: Vec<TxDataField>,
}

fn le16(data: &[u8]) -> u16 {
    data[0] as u16 | ((data[1] as u16) << 8)
}

fn txdata_xor_high_byte(hi: u8) -> bool {
    (0x50..=0x7f).contains(&hi) || hi >= 0x80
}

fn decoded_byte_texts(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if !data.is_empty() && !data.iter().any(|b| *b == 0) {
        if let Ok(text) = std::str::from_utf8(data) {
            if reasonable_text(text) {
                out.push(text.to_string());
            }
        }
        #[cfg(feature = "encoding_rs")]
        {
            let (decoded, _, had_errors) = encoding_rs::GB18030.decode(data);
            if !had_errors {
                let text = decoded.to_string();
                if reasonable_text(&text) && !out.contains(&text) {
                    out.push(text);
                }
            }
        }
    }
    out
}

fn reasonable_text(text: &str) -> bool {
    if text.is_empty() || text.len() > 65536 {
        return false;
    }
    if text.contains('\u{fffd}') {
        return false;
    }
    !text
        .chars()
        .any(|ch| (ch as u32) < 0x20 && ch != '\n' && ch != '\r' && ch != '\t')
}

fn decode_txdata_obfuscated_bytes(data: &[u8]) -> Vec<u8> {
    if data.is_empty() || data.len() > 0xffff {
        return Vec::new();
    }
    let len = data.len() as u16;
    let key = (len as u8) ^ ((len >> 8) as u8);
    data.iter().map(|b| !*b ^ key).collect()
}

fn decode_utf16le_text(data: &[u8]) -> String {
    if data.len() < 2 || data.len() % 2 != 0 {
        return String::new();
    }
    let mut units = Vec::with_capacity(data.len() / 2);
    for chunk in data.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            return String::new();
        }
        units.push(unit);
    }
    let Ok(text) = String::from_utf16(&units) else {
        return String::new();
    };
    if reasonable_text(&text) {
        text
    } else {
        String::new()
    }
}

fn txdata_numeric_name(name: &str) -> bool {
    name.starts_with("dw")
        || name.starts_with('n')
        || name.starts_with('b')
        || name.starts_with('c')
        || name.starts_with('w')
        || name.starts_with('u')
}

fn looks_like_structured_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.len() < 4 {
        return false;
    }
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("UserData")
        || trimmed.starts_with("OSRoot:")
        || trimmed.starts_with("sysres:")
        || trimmed.contains('/')
        || trimmed.contains('\\')
    {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    let has_struct_ext = [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".amr", ".mp3", ".wav", ".m4a", ".mp4",
        ".mov", ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".zip", ".rar", ".7z",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext));
    if has_struct_ext {
        return true;
    }
    let hex = trimmed.bytes().filter(|c| c.is_ascii_hexdigit()).count();
    trimmed.len() >= 16 && hex * 100 / trimmed.len() >= 80
}

fn printable_ascii_text(data: &[u8]) -> String {
    if data.is_empty() || data.len() > 4096 {
        return String::new();
    }
    if !data
        .iter()
        .all(|b| matches!(*b, 0x09 | 0x0a | 0x0d | 0x20..=0x7e))
    {
        return String::new();
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return String::new();
    };
    if reasonable_text(text) {
        text.to_string()
    } else {
        String::new()
    }
}

pub fn decode_txdata_string(data: &[u8]) -> String {
    let plain = decode_txdata_obfuscated_bytes(data);
    if plain.is_empty() {
        return String::new();
    }
    let ascii = printable_ascii_text(&plain);
    if !ascii.is_empty() {
        return ascii;
    }
    let wide = decode_utf16le_text(&plain);
    if !wide.is_empty() {
        return wide;
    }
    decoded_byte_texts(&plain)
        .into_iter()
        .next()
        .unwrap_or_default()
}

pub fn decode_fixed_high_byte_string(data: &[u8]) -> String {
    decode_txdata_string(data)
}

pub fn decode_fixed_high_byte_strings(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let full = decode_txdata_string(data);
    if !full.is_empty() {
        out.push(full);
    }
    let mut i = 0usize;
    while i + 1 < data.len() && out.len() < 32 {
        let hi = data[i + 1];
        if !txdata_xor_high_byte(hi) {
            i += 1;
            continue;
        }
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
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{decode_txdata_fields, decode_txdata_string};

    fn encode_txdata_string(text: &str) -> Vec<u8> {
        let plain = text.as_bytes();
        let len = plain.len() as u16;
        let key = (len as u8) ^ ((len >> 8) as u8);
        plain.iter().map(|b| !*b ^ key).collect()
    }

    fn txdata_record(fields: Vec<(u8, &str, Vec<u8>)>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for (header, name, value) in fields {
            let encoded_name = encode_txdata_string(name);
            out.push(header);
            out.extend_from_slice(&(encoded_name.len() as u16).to_le_bytes());
            out.extend_from_slice(&encoded_name);
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(&value);
        }
        out
    }

    #[test]
    fn decodes_txdata_utf16_string_with_length_key() {
        let raw = [
            0xb1, 0xf1, 0x81, 0xf1, 0x9d, 0xf1, 0xd1, 0xf1, 0x18, 0x94, 0xfb, 0xbf, 0x8c, 0xa8,
        ];
        assert_eq!(decode_txdata_string(&raw), "@pl 早上好");
    }

    #[test]
    fn decodes_txdata_ascii_field_name_before_utf16_fallback() {
        let plain = b"exclusiveLabel";
        let key = (plain.len() as u16 as u8) ^ (((plain.len() as u16) >> 8) as u8);
        let raw: Vec<u8> = plain.iter().map(|b| !*b ^ key).collect();
        assert_eq!(decode_txdata_string(&raw), "exclusiveLabel");
    }

    #[test]
    fn numeric_fields_do_not_emit_text_candidates() {
        let raw = txdata_record(vec![
            (6, "dwMsgSeq", 806830u32.to_le_bytes().to_vec()),
            (1, "bShowBySessionPanel", 0u32.to_le_bytes().to_vec()),
        ]);
        let decoded = decode_txdata_fields(&raw);
        assert!(decoded.complete);
        assert_eq!(decoded.fields.len(), 2);
        assert!(decoded.fields[0].value_texts.is_empty());
        assert!(decoded.fields[1].value_texts.is_empty());
    }

    #[test]
    fn string_field_still_decodes_abstract_text() {
        let raw = txdata_record(vec![(
            8,
            "bsAbstractText",
            encode_txdata_string("未沾血的石器将你移出群聊。"),
        )]);
        let decoded = decode_txdata_fields(&raw);
        assert!(decoded.complete);
        assert_eq!(
            decoded.fields[0].value_texts,
            vec!["未沾血的石器将你移出群聊。"]
        );
    }

    #[test]
    fn buffer_field_keeps_structured_path_text() {
        let raw = txdata_record(vec![(
            9,
            "bufPicInfoServerPath",
            encode_txdata_string("UserDataImage:C2C\\Image2\\abc.png"),
        )]);
        let decoded = decode_txdata_fields(&raw);
        assert!(decoded.complete);
        assert_eq!(
            decoded.fields[0].value_texts,
            vec!["UserDataImage:C2C\\Image2\\abc.png"]
        );
    }
}

pub fn decode_txdata_fields(data: &[u8]) -> TxDataRecord {
    if data.len() < 2 {
        return TxDataRecord::default();
    }
    let count = le16(&data[..2]);
    if count == 0 {
        return TxDataRecord {
            count,
            complete: data.len() == 2,
            fields: Vec::new(),
        };
    }
    if count > 200 {
        return TxDataRecord {
            count,
            complete: false,
            fields: Vec::new(),
        };
    }
    let mut pos = 2usize;
    let mut fields = Vec::new();
    let mut complete = true;
    for _ in 0..count {
        if pos + 3 > data.len() {
            complete = false;
            break;
        }
        let header = data[pos];
        let name_len = le16(&data[pos + 1..pos + 3]) as usize;
        pos += 3;
        if name_len > data.len() - pos {
            complete = false;
            break;
        }
        let name = decode_fixed_high_byte_string(&data[pos..pos + name_len]);
        pos += name_len;
        if pos + 4 > data.len() {
            complete = false;
            break;
        }
        let value_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        pos += 4;
        let n = value_len as usize;
        if n > data.len() - pos {
            complete = false;
            break;
        }
        let value = data[pos..pos + n].to_vec();
        pos += n;
        let numeric = matches!(header, 1 | 2 | 3 | 4 | 6) && txdata_numeric_name(&name);
        let mut value_text_len = 0usize;
        let value_texts = if numeric {
            Vec::new()
        } else if header == 8 {
            let full = decode_fixed_high_byte_string(&value);
            if !full.is_empty() {
                value_text_len = value.len();
                vec![full]
            } else {
                Vec::new()
            }
        } else if header == 9 || name.starts_with("buf") || name.starts_with("guid") {
            decode_fixed_high_byte_strings(&value)
                .into_iter()
                .filter(|text| looks_like_structured_text(text))
                .collect()
        } else {
            Vec::new()
        };
        fields.push(TxDataField {
            header,
            name,
            value_len,
            value,
            value_texts,
            value_text_len,
        });
    }
    TxDataRecord {
        count,
        complete: complete && pos == data.len(),
        fields,
    }
}

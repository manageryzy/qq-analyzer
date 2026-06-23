use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use flate2::read::ZlibDecoder;
use serde::Serialize;
use serde_json::Value;
use std::io::Read;

use crate::txdata_codec::{decode_txdata_fields, TxDataField};

fn u32v(x: u32) -> u32 {
    x
}

fn tea_decipher_block(block: &[u8], key: &[u8; 16]) -> [u8; 8] {
    let mut y = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
    let mut z = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);
    let k = [
        u32::from_be_bytes([key[0], key[1], key[2], key[3]]),
        u32::from_be_bytes([key[4], key[5], key[6], key[7]]),
        u32::from_be_bytes([key[8], key[9], key[10], key[11]]),
        u32::from_be_bytes([key[12], key[13], key[14], key[15]]),
    ];
    let mut sum = 0xe3779b90u32;
    for _ in 0..16 {
        z = z.wrapping_sub(
            u32v((y << 4).wrapping_add(k[2])) ^ y.wrapping_add(sum) ^ (y >> 5).wrapping_add(k[3]),
        );
        y = y.wrapping_sub(
            u32v((z << 4).wrapping_add(k[0])) ^ z.wrapping_add(sum) ^ (z >> 5).wrapping_add(k[1]),
        );
        sum = sum.wrapping_sub(0x9e3779b9);
    }
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&y.to_be_bytes());
    out[4..].copy_from_slice(&z.to_be_bytes());
    out
}

fn qq_tea_decrypt(cipher: &[u8], key: &[u8; 16], salt_bytes: usize) -> anyhow::Result<Vec<u8>> {
    if cipher.len() <= 15 || cipher.len() % 8 != 0 {
        anyhow::bail!("cipher length must be >15 and multiple of 8");
    }
    let mut plain = tea_decipher_block(&cipher[..8], key);
    let pad = (plain[0] & 7) as usize;
    let mut out_len = cipher
        .len()
        .checked_sub(pad + 10)
        .ok_or_else(|| anyhow::anyhow!("negative plaintext length"))?;
    let mut out = Vec::with_capacity(out_len);
    let mut pos = pad + 1;
    let mut crypt = 8usize;
    let mut pre_cipher = [0u8; 8];

    let next_block = |plain: &mut [u8; 8],
                      crypt: &mut usize,
                      pre_cipher: &mut [u8; 8]|
     -> anyhow::Result<usize> {
        if *crypt >= cipher.len() {
            anyhow::bail!("cipher ended before next block");
        }
        let block = &cipher[*crypt..*crypt + 8];
        let mut mixed = [0u8; 8];
        for i in 0..8 {
            mixed[i] = plain[i] ^ block[i];
        }
        pre_cipher.copy_from_slice(&cipher[*crypt - 8..*crypt]);
        *plain = tea_decipher_block(&mixed, key);
        *crypt += 8;
        Ok(0)
    };

    let mut skipped = 0usize;
    while skipped < salt_bytes {
        if pos < 8 {
            pos += 1;
            skipped += 1;
        } else if crypt < cipher.len() {
            pos = next_block(&mut plain, &mut crypt, &mut pre_cipher)?;
        } else {
            anyhow::bail!("cipher ended while skipping salt");
        }
    }

    while out_len > 0 {
        if pos < 8 {
            out.push(plain[pos] ^ pre_cipher[pos]);
            pos += 1;
            out_len -= 1;
        } else if crypt < cipher.len() {
            pos = next_block(&mut plain, &mut crypt, &mut pre_cipher)?;
        } else {
            anyhow::bail!("cipher ended while reading plaintext");
        }
    }
    Ok(out)
}

fn zlib_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

fn clean_key_hex(text: &str) -> String {
    text.chars()
        .filter(|ch| !matches!(ch, ' ' | ':' | '-'))
        .collect()
}

fn key_from_hex(text: &str) -> Option<[u8; 16]> {
    let hex = clean_key_hex(text);
    if hex.len() != 32 || hex == "0".repeat(32) {
        return None;
    }
    let mut key = [0u8; 16];
    for i in 0..16 {
        key[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(key)
}

fn latest_key(path: &Path) -> Option<[u8; 16]> {
    if let Ok(env_key) = env::var("PCQQ_INFOSTORAGE_KEY_HEX") {
        if let Some(key) = key_from_hex(&env_key) {
            return Some(key);
        }
    }
    let text = fs::read_to_string(path).ok()?;
    let mut out = None;
    for line in text.lines() {
        let Ok(item) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(value) = item["key_hex"].as_str().and_then(key_from_hex) {
            out = Some(value);
        }
    }
    out
}

#[derive(Clone)]
pub struct InfoStorage {
    root: PathBuf,
    key: Option<[u8; 16]>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct InfoStorageDecodedField {
    pub index: usize,
    pub header: u8,
    pub name: String,
    pub value_len: u32,
    pub uint_value: Option<u32>,
    pub texts: Vec<String>,
    pub role: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GroupMemberProfile {
    pub uin: String,
    pub display_name: String,
    pub display_name_field: Option<usize>,
    pub title: String,
    pub title_field: Option<usize>,
    pub head_index: Option<u32>,
    pub faceshop_face_id: Option<u32>,
    pub fields: Vec<InfoStorageDecodedField>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ContactProfile {
    pub uin: String,
    pub faceshop_face_id: Option<u32>,
    pub custom_head_file: String,
    pub custom_head_file_field: String,
    pub fields: Vec<InfoStorageDecodedField>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GroupProfile {
    pub group_id: String,
    pub name: String,
    pub memo: String,
    pub finger_memo: String,
    pub group_face: Option<u32>,
    pub use_custom_face: Option<u32>,
    pub custom_image_type: Option<u32>,
    pub image_file_path: String,
    pub head_history_paths: Vec<String>,
    pub file_hash_hex: String,
    pub user_head_timestamp_v2: Option<u32>,
    pub fields: Vec<InfoStorageDecodedField>,
}

impl InfoStorage {
    pub fn new(root: PathBuf, key_log: PathBuf) -> Self {
        Self {
            root,
            key: latest_key(&key_log),
        }
    }

    pub fn available(&self) -> bool {
        self.key.is_some() && self.root.exists()
    }

    pub fn label(&self, kind: &str, ident: &str) -> anyhow::Result<String> {
        match kind {
            "group" => self.group_label(ident),
            "discuss" => self.entry_label("discuss/remark.db", ident, &[0, 1, 2, 3, 4, 5], false),
            "buddy" => self
                .entry_label("Contact/Remark.db", ident, &[0, 1, 2, 3, 4, 5], false)?
                .or_else_nonempty(|| {
                    self.entry_label("Contact/PublicInfo.db", ident, &[0, 1, 2, 3, 4, 5], false)
                }),
            _ => Ok(String::new()),
        }
    }

    pub fn group_member_profiles_for(
        &self,
        group_id: &str,
        wanted: &HashSet<String>,
    ) -> anyhow::Result<HashMap<String, GroupMemberProfile>> {
        if !group_id.chars().all(|ch| ch.is_ascii_digit()) {
            return Ok(HashMap::new());
        }
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let rel = format!("User/Group{group_id}.db");
        let entries = self
            .entries_filtered(&rel, Some(wanted))
            .unwrap_or_default();
        let mut out = HashMap::new();
        for (uin, fields) in entries {
            let profile = group_member_profile(&uin, &fields)?;
            if !profile.display_name.is_empty()
                || !profile.title.is_empty()
                || !profile.fields.is_empty()
            {
                out.insert(uin, profile);
            }
        }
        Ok(out)
    }

    pub fn entries_for_stream(
        &self,
        rel: &str,
        wanted: Option<&HashSet<String>>,
    ) -> anyhow::Result<HashMap<String, Vec<TxDataField>>> {
        self.entries_filtered(rel, wanted)
    }

    pub fn contact_profiles_for(
        &self,
        wanted: &HashSet<String>,
    ) -> anyhow::Result<HashMap<String, ContactProfile>> {
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let mut out: HashMap<String, ContactProfile> = HashMap::new();
        for rel in [
            "Contact/QQInfo.db",
            "Contact/NonRelationQQInfo.db",
            "Contact/PublicInfo.db",
            "Contact/Remark.db",
        ] {
            let entries = self.entries_filtered(rel, Some(wanted)).unwrap_or_default();
            for (uin, fields) in entries {
                let profile = out.entry(uin.clone()).or_insert_with(|| ContactProfile {
                    uin,
                    ..ContactProfile::default()
                });
                merge_contact_profile(profile, &fields)?;
            }
        }
        Ok(out)
    }

    pub fn group_profile(&self, group_id: &str) -> anyhow::Result<GroupProfile> {
        let mut wanted = HashSet::new();
        wanted.insert(group_id.to_string());
        let entries = self
            .entries_filtered("Group/Basic.db", Some(&wanted))
            .unwrap_or_default();
        let Some(fields) = entries.get(group_id) else {
            return Ok(GroupProfile {
                group_id: group_id.to_string(),
                ..GroupProfile::default()
            });
        };
        let head_history_paths = fields
            .iter()
            .find(|field| field.name == "arrayGroupHeadHistory")
            .map(|field| field.value_texts.clone())
            .unwrap_or_default();
        Ok(GroupProfile {
            group_id: group_id.to_string(),
            name: named_text(fields, "strGroupName")?.unwrap_or_default(),
            memo: named_text(fields, "strGroupMemo")?.unwrap_or_default(),
            finger_memo: named_text(fields, "strFingerMemo")?.unwrap_or_default(),
            group_face: named_uint(fields, "wGroupFace"),
            use_custom_face: named_uint(fields, "cUseCustomFace"),
            custom_image_type: named_uint(fields, "enumCostomImageType"),
            image_file_path: named_text(fields, "strImageFilePath")?.unwrap_or_default(),
            head_history_paths,
            file_hash_hex: named_bytes(fields, "bufFileHash").unwrap_or_default(),
            user_head_timestamp_v2: named_uint(fields, "dwUserHeadTimestamp_v2"),
            fields: decoded_fields(fields)?,
        })
    }

    pub fn friend_social_image_profiles_for(
        &self,
        wanted: &HashSet<String>,
    ) -> anyhow::Result<HashMap<String, ContactProfile>> {
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let entries = self
            .entries_filtered("dbImage.db", Some(wanted))
            .unwrap_or_default();
        let mut out = HashMap::new();
        for (uin, fields) in entries {
            let custom_head_file = named_text(&fields, "sSize100")?.unwrap_or_default();
            out.insert(
                uin.clone(),
                ContactProfile {
                    uin,
                    faceshop_face_id: None,
                    custom_head_file,
                    custom_head_file_field: "FriendSocial.db/FriendSocial/dbImage.db:sSize100"
                        .to_string(),
                    fields: decoded_fields(&fields)?,
                },
            );
        }
        Ok(out)
    }

    fn group_label(&self, ident: &str) -> anyhow::Result<String> {
        let entries = self.entries("Group/Basic.db").unwrap_or_default();
        let Some(fields) = entries.get(ident) else {
            return Ok(String::new());
        };
        for name in ["string_long_group_name", "strGroupName", "strGroupName1"] {
            if let Some(text) = named_values(fields)?
                .get(name)
                .and_then(|v| v.first())
                .cloned()
            {
                if usable_label(&text, ident, false) {
                    return Ok(text);
                }
            }
        }
        priority_field_text(fields, &[2], ident, false)
    }

    fn entry_label(
        &self,
        rel: &str,
        ident: &str,
        order: &[usize],
        allow_numeric: bool,
    ) -> anyhow::Result<String> {
        let entries = self.entries(rel).unwrap_or_default();
        let Some(fields) = entries.get(ident) else {
            return Ok(String::new());
        };
        priority_field_text(fields, order, ident, allow_numeric)
    }

    fn entries(&self, rel: &str) -> anyhow::Result<HashMap<String, Vec<TxDataField>>> {
        self.entries_filtered(rel, None)
    }

    fn entries_filtered(
        &self,
        rel: &str,
        wanted: Option<&HashSet<String>>,
    ) -> anyhow::Result<HashMap<String, Vec<TxDataField>>> {
        let mut out: HashMap<String, Vec<TxDataField>> = HashMap::new();
        let Some(key) = self.key else {
            return Ok(out);
        };
        let path = self.root.join(rel);
        let data = fs::read(path)?;
        if data.len() >= 5 && data.starts_with(b"ES\x01\x01") {
            parse_inner_entries(&data[4..], &mut out, wanted);
            return Ok(out);
        }
        if data.len() < 8 || !(data.starts_with(b"ES\x01\x03") || data.starts_with(b"ES\x01\x02")) {
            return Ok(HashMap::new());
        }
        let mut pos = 8usize;
        while pos + 8 <= data.len() {
            let enc_len =
                u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            pos += 8;
            if enc_len == 0 || pos + enc_len > data.len() {
                break;
            }
            let enc = &data[pos..pos + enc_len];
            pos += enc_len;
            let Ok(compressed) = qq_tea_decrypt(enc, &key, 2) else {
                continue;
            };
            let Ok(inflated) = zlib_decompress(&compressed) else {
                continue;
            };
            parse_inner_entries(&inflated, &mut out, wanted);
        }
        Ok(out)
    }
}

fn group_member_profile(uin: &str, fields: &[TxDataField]) -> anyhow::Result<GroupMemberProfile> {
    let decoded_fields = decoded_fields(fields)?;
    // IM.dll CTXGroupMemberMgr reads strCardName as the display name and
    // handles cGroupMemIdentity separately for owner/admin identity.
    let (display_name, display_name_field) =
        first_field_text(fields, &[3, 2, 4, 5, 6, 7, 8, 9], uin, true)?;
    let (title, title_field) = first_field_text(fields, &[1], uin, true)?;
    let mut profile = GroupMemberProfile {
        uin: uin.to_string(),
        display_name,
        display_name_field,
        title,
        title_field,
        head_index: named_uint(fields, "wHeadIndex"),
        faceshop_face_id: named_uint(fields, "dwFaceshopFaceId"),
        fields: decoded_fields,
    };
    for field in &mut profile.fields {
        field.role = match Some(field.index) {
            value if value == profile.display_name_field => "display_name".to_string(),
            value if value == profile.title_field => "title".to_string(),
            _ => field.role.clone(),
        };
    }
    Ok(profile)
}

fn merge_contact_profile(
    profile: &mut ContactProfile,
    fields: &[TxDataField],
) -> anyhow::Result<()> {
    if profile.faceshop_face_id.is_none() {
        profile.faceshop_face_id = named_uint(fields, "dwFaceshopFaceId");
    }
    if profile.custom_head_file.is_empty() {
        for name in [
            "strCustomHeadFile140",
            "strCustomHeadFile100",
            "strCustomHeadFileOrigin",
            "strCustomHeadFile",
        ] {
            if let Some(value) = named_text(fields, name)? {
                profile.custom_head_file = value;
                profile.custom_head_file_field = name.to_string();
                break;
            }
        }
    }
    if profile.fields.is_empty() {
        profile.fields = decoded_fields(fields)?;
    }
    Ok(())
}

fn named_text(fields: &[TxDataField], name: &str) -> anyhow::Result<Option<String>> {
    for field in fields {
        if field.name == name && field.header == 8 {
            if let Some(value) = field_texts(field)?
                .into_iter()
                .find(|value| !value.is_empty())
            {
                return Ok(Some(value));
            }
        }
    }
    Ok(None)
}

fn named_bytes(fields: &[TxDataField], name: &str) -> Option<String> {
    fields.iter().find_map(|field| {
        if field.name == name {
            Some(
                field
                    .value
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            )
        } else {
            None
        }
    })
}

fn named_uint(fields: &[TxDataField], name: &str) -> Option<u32> {
    fields.iter().find_map(|field| {
        if field.name != name {
            return None;
        }
        match (field.header, field.value.as_slice()) {
            (1 | 6, [a, b, c, d]) => Some(u32::from_le_bytes([*a, *b, *c, *d])),
            (2, [v]) => Some(*v as u32),
            (3 | 4, [a, b]) => Some(u16::from_le_bytes([*a, *b]) as u32),
            _ => None,
        }
    })
}

trait NonEmptyString {
    fn or_else_nonempty<F: FnOnce() -> anyhow::Result<String>>(
        self,
        f: F,
    ) -> anyhow::Result<String>;
}

impl NonEmptyString for String {
    fn or_else_nonempty<F: FnOnce() -> anyhow::Result<String>>(
        self,
        f: F,
    ) -> anyhow::Result<String> {
        if self.is_empty() {
            f()
        } else {
            Ok(self)
        }
    }
}

fn parse_inner_entries(
    buf: &[u8],
    out: &mut HashMap<String, Vec<TxDataField>>,
    wanted: Option<&HashSet<String>>,
) {
    let mut pos = 0usize;
    while pos + 5 <= buf.len() {
        let tag = buf[pos];
        let size =
            u32::from_be_bytes([buf[pos + 1], buf[pos + 2], buf[pos + 3], buf[pos + 4]]) as usize;
        pos += 5;
        if size == 0 || pos + size > buf.len() {
            break;
        }
        let payload = &buf[pos..pos + size];
        pos += size;
        if tag != 2 {
            continue;
        }
        let mut p = 0usize;
        while p + 8 <= payload.len() {
            let name_len =
                u32::from_be_bytes([payload[p], payload[p + 1], payload[p + 2], payload[p + 3]])
                    as usize;
            p += 4;
            if p + name_len + 4 > payload.len() {
                break;
            }
            let name = String::from_utf8_lossy(&payload[p..p + name_len]).to_string();
            p += name_len;
            let data_len =
                u32::from_be_bytes([payload[p], payload[p + 1], payload[p + 2], payload[p + 3]])
                    as usize;
            p += 4;
            if p + data_len > payload.len() {
                break;
            }
            if wanted.is_some_and(|set| !set.contains(&name)) {
                p += data_len;
                continue;
            }
            let data = &payload[p..p + data_len];
            p += data_len;
            if data.starts_with(b"TD\x01\x01") || data.starts_with(b"TA\x01\x01") {
                let rec = decode_txdata_fields(&data[4..]);
                if !rec.fields.is_empty() {
                    out.entry(name).or_default().extend(rec.fields);
                }
            }
        }
    }
}

fn field_texts(field: &TxDataField) -> anyhow::Result<Vec<String>> {
    if field.header == 8 && field.value_text_len != 0 && field.value_text_len != field.value.len() {
        anyhow::bail!(
            "partial TXData string decode for field {:?}: consumed {} of {} bytes, value_hex={}",
            field.name,
            field.value_text_len,
            field.value.len(),
            hex_prefix(&field.value, 96)
        );
    }
    Ok(field
        .value_texts
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

fn usable_label(text: &str, ident: &str, allow_numeric: bool) -> bool {
    let t = text.trim();
    if t.is_empty() || t == ident {
        return false;
    }
    if t.len() > 80 || t.contains('\0') || t.contains('\\') || t.contains('/') {
        return false;
    }
    if !allow_numeric && t.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    !t.chars().any(|ch| (ch as u32) < 0x20)
}

fn priority_field_text(
    fields: &[TxDataField],
    order: &[usize],
    ident: &str,
    allow_numeric: bool,
) -> anyhow::Result<String> {
    Ok(first_field_text(fields, order, ident, allow_numeric)?.0)
}

fn first_field_text(
    fields: &[TxDataField],
    order: &[usize],
    ident: &str,
    allow_numeric: bool,
) -> anyhow::Result<(String, Option<usize>)> {
    for idx in order {
        if let Some(field) = fields.get(*idx) {
            if field.header != 8 {
                continue;
            }
            for text in field_texts(field)? {
                if usable_label(&text, ident, allow_numeric) {
                    return Ok((text, Some(*idx)));
                }
            }
        }
    }
    Ok((String::new(), None))
}

fn decoded_fields(fields: &[TxDataField]) -> anyhow::Result<Vec<InfoStorageDecodedField>> {
    let mut out = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let texts = if field.header == 8 {
            field_texts(field)?
        } else {
            Vec::new()
        };
        let uint_value = match (field.header, field.value.as_slice()) {
            (1 | 6, [a, b, c, d]) => Some(u32::from_le_bytes([*a, *b, *c, *d])),
            (2, [v]) => Some(*v as u32),
            (3, [a, b]) => Some(u16::from_le_bytes([*a, *b]) as u32),
            _ => None,
        };
        out.push(InfoStorageDecodedField {
            index,
            header: field.header,
            name: field.name.clone(),
            value_len: field.value_len,
            uint_value,
            texts,
            role: String::new(),
        });
    }
    Ok(out)
}

fn named_values(fields: &[TxDataField]) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for field in fields {
        if !field.name.is_empty() {
            for text in field_texts(field)? {
                out.entry(field.name.clone()).or_default().push(text);
            }
        }
        for rec in nested_txdata(&field.value) {
            for nested in rec {
                if nested.name.is_empty() {
                    continue;
                }
                for text in field_texts(&nested)? {
                    out.entry(nested.name.clone()).or_default().push(text);
                }
            }
        }
    }
    Ok(out)
}

fn nested_txdata(data: &[u8]) -> Vec<Vec<TxDataField>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= data.len() {
        if (data[i..].starts_with(b"TD\x01\x01") || data[i..].starts_with(b"TA\x01\x01"))
            && i + 4 <= data.len()
        {
            let rec = decode_txdata_fields(&data[i + 4..]);
            if !rec.fields.is_empty() {
                out.push(rec.fields);
            }
        }
        i += 1;
    }
    out
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

use std::fs;
use std::path::{Path, PathBuf};

use crate::txdata_codec;

pub(crate) fn asset_index_files(root: &Path, account: &str) -> Vec<PathBuf> {
    vec![
        root.join("qq-analyzer")
            .join("output")
            .join(account)
            .join("extracted-cfb")
            .join("FileInfo.db")
            .join("FileTrans")
            .join("FileIndex")
            .join("FileIndex.dat"),
        root.join("qq-analyzer")
            .join("output")
            .join(account)
            .join("extracted-cfb")
            .join("FileTransferMsg.db")
            .join("Buddy")
            .join("BuddyFileInfo.dat"),
        root.join("qq-analyzer")
            .join("output")
            .join(account)
            .join("extracted-cfb")
            .join("FileTransferMsg.db")
            .join("Discuss")
            .join("DiscussFileInfo.dat"),
        root.join("qq-analyzer")
            .join("output")
            .join(account)
            .join("extracted-cfb")
            .join("Misc.db")
            .join("GroupFileResumeInfo.dat"),
        root.join("qq-analyzer")
            .join("output")
            .join(account)
            .join("extracted-cfb")
            .join("Misc.db")
            .join("GroupFileXTFResumeInfo.dat"),
    ]
}

pub(crate) fn collect_index_paths(
    root: &Path,
    account: &str,
    data: &[u8],
    tokens: &[String],
    out: &mut Vec<PathBuf>,
) {
    let mut pos = 0usize;
    while pos + 6 <= data.len() {
        if data[pos..].starts_with(b"TA\x01\x01") {
            if let Some(len) = ta_record_len(data, pos) {
                collect_ta_paths(root, account, &data[pos..pos + len], tokens, out);
                pos += len.max(1);
                continue;
            }
        }
        if data[pos..].starts_with(b"TD\x01\x01") {
            if let Some(len) = td_record_len(data, pos) {
                let rec = txdata_codec::decode_txdata_fields(&data[pos + 4..pos + len]);
                collect_record_paths(root, account, &rec.fields, tokens, out);
                for field in &rec.fields {
                    collect_index_paths(root, account, &field.value, tokens, out);
                }
                pos += len.max(1);
                continue;
            }
        }
        pos += 1;
    }
}

fn collect_ta_paths(
    root: &Path,
    account: &str,
    data: &[u8],
    tokens: &[String],
    out: &mut Vec<PathBuf>,
) {
    if data.len() < 8 {
        return;
    }
    let count = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let mut pos = 8usize;
    for _ in 0..count.min(100_000) {
        if pos + 5 > data.len() {
            break;
        }
        let value_len =
            u32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]])
                as usize;
        pos += 5;
        if pos + value_len > data.len() {
            break;
        }
        collect_index_paths(root, account, &data[pos..pos + value_len], tokens, out);
        pos += value_len;
    }
}

fn collect_record_paths(
    root: &Path,
    account: &str,
    fields: &[txdata_codec::TxDataField],
    tokens: &[String],
    out: &mut Vec<PathBuf>,
) {
    let mut record_texts = Vec::new();
    let mut path_texts = Vec::new();
    for field in fields {
        for text in &field.value_texts {
            if !text.is_empty() {
                record_texts.push(text.clone());
                if matches!(
                    field.name.as_str(),
                    "bsIndexPath" | "strFinalFile" | "strFinalFilePath" | "strResumeName"
                ) {
                    path_texts.push(text.clone());
                }
            }
        }
    }
    if path_texts.is_empty() || !record_matches_tokens(&record_texts, tokens) {
        return;
    }
    for text in path_texts {
        for path in indexed_path_candidates(root, account, &text) {
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
}

fn record_matches_tokens(record_texts: &[String], tokens: &[String]) -> bool {
    tokens.iter().any(|token| {
        let token = token.trim().trim_start_matches('/').to_lowercase();
        if token.is_empty() || token == "{}" {
            return false;
        }
        record_texts.iter().any(|text| {
            let text_l = text.to_lowercase();
            text_l == token || text_l.contains(&token)
        })
    })
}

fn indexed_path_candidates(root: &Path, account: &str, text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut value = text.trim().to_string();
    if let Some(rest) = value.strip_prefix("OSRoot:") {
        value = rest.to_string();
    }
    let lower = value.to_lowercase();
    let account_l = account.to_lowercase();
    let tencent_needle = format!("\\tencent files\\{}\\", account_l);
    if let Some(pos) = lower.find(&tencent_needle) {
        let rel = &value[pos + tencent_needle.len()..];
        out.push(root.join(account).join(path_from_qq_rel(rel)));
    }
    let account_needle = format!("\\{}\\", account_l);
    if let Some(pos) = lower.find(&account_needle) {
        let rel = &value[pos + account_needle.len()..];
        out.push(root.join(account).join(path_from_qq_rel(rel)));
    }
    out.push(PathBuf::from(value));
    out
}

fn td_record_len(data: &[u8], start: usize) -> Option<usize> {
    if start + 6 > data.len() || !data[start..].starts_with(b"TD\x01\x01") {
        return None;
    }
    let count = u16::from_le_bytes([data[start + 4], data[start + 5]]) as usize;
    if count == 0 || count > 200 {
        return None;
    }
    let mut pos = start + 6;
    for _ in 0..count {
        if pos + 3 > data.len() {
            return None;
        }
        let name_len = u16::from_le_bytes([data[pos + 1], data[pos + 2]]) as usize;
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

pub(crate) fn local_asset_candidates(root: &Path, account: &str, text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let normalized = text.trim().replace('/', "\\");
    if let Some(rest) = normalized.strip_prefix("UserDataImage:") {
        let rel = path_from_qq_rel(rest);
        push_with_thumbnail(&mut out, root.join(account).join("Image").join(&rel));
        if rest.starts_with("Group\\") {
            if let Some(after_group) = rest.strip_prefix("Group\\") {
                push_with_thumbnail(
                    &mut out,
                    root.join(account)
                        .join("Image")
                        .join("Group")
                        .join("thumbnail")
                        .join(path_from_qq_rel(after_group)),
                );
                push_group2_image_candidates(&mut out, root, account, after_group);
            }
        }
    } else if let Some(rest) = normalized.strip_prefix("UserDataCustomFace:") {
        push_with_thumbnail(
            &mut out,
            root.join(account)
                .join("CustomFace")
                .join(path_from_qq_rel(rest)),
        );
        push_with_thumbnail(
            &mut out,
            root.join("qq-analyzer")
                .join("output")
                .join(account)
                .join("extracted-cfb")
                .join("CustomFace.db")
                .join(path_from_qq_rel(rest)),
        );
    } else if let Some(rest) = normalized.strip_prefix("FaceStore:") {
        let rel = path_from_qq_rel(rest);
        push_with_thumbnail(
            &mut out,
            root.join("qq-analyzer")
                .join("output")
                .join(account)
                .join("extracted-cfb")
                .join("FaceStore.db")
                .join(&rel),
        );
        if let Some(name) = rel.file_name().and_then(|s| s.to_str()) {
            if let Some(stem) = name.strip_suffix(".fixed") {
                push_with_thumbnail(
                    &mut out,
                    root.join("qq-analyzer")
                        .join("output")
                        .join(account)
                        .join("extracted-cfb")
                        .join("FaceStore.db")
                        .join(stem),
                );
            } else {
                push_with_thumbnail(
                    &mut out,
                    root.join("qq-analyzer")
                        .join("output")
                        .join(account)
                        .join("extracted-cfb")
                        .join("FaceStore.db")
                        .join(format!("{name}.fixed")),
                );
            }
        }
    } else if let Some(rest) = normalized.strip_prefix("SysFaceResFileSystem:") {
        let rel = path_from_qq_rel(rest);
        push_with_thumbnail(
            &mut out,
            root.join("qq-analyzer")
                .join("output")
                .join(account)
                .join("extracted-cfb")
                .join("SysFaceResFileSystem")
                .join(&rel),
        );
        push_with_thumbnail(
            &mut out,
            root.join("qq-analyzer")
                .join("output")
                .join(account)
                .join("extracted-cfb-current")
                .join("SysFaceResFileSystem")
                .join(&rel),
        );
    } else if let Some(rest) = normalized.strip_prefix("UserDataRoot:") {
        out.push(root.join(account).join(path_from_qq_rel(rest)));
    } else if let Some(rest) = normalized.strip_prefix("UserDataFileRecv:") {
        out.push(
            root.join(account)
                .join("FileRecv")
                .join(path_from_qq_rel(rest)),
        );
        out.push(root.join(account).join(path_from_qq_rel(rest)));
        if !rest.contains('\\') && looks_like_filename(rest) {
            push_file_recv_subdir_candidates(&mut out, root, account, rest);
        }
    } else if normalized.contains('\\') || normalized.contains('/') {
        out.push(root.join(account).join(path_from_qq_rel(&normalized)));
        out.push(
            root.join(account)
                .join("FileRecv")
                .join(path_from_qq_rel(&normalized)),
        );
    } else if looks_like_filename(&normalized) {
        out.push(root.join(account).join("FileRecv").join(&normalized));
        out.push(root.join(account).join(&normalized));
        out.push(root.join(account).join("Audio").join(&normalized));
        out.push(root.join(account).join("Video").join(&normalized));
        out.push(root.join(account).join("GroupFile").join(&normalized));
        push_file_recv_subdir_candidates(&mut out, root, account, &normalized);
    } else {
        push_system_face_candidates(&mut out, root, account, &normalized);
    }
    out
}

fn push_system_face_candidates(out: &mut Vec<PathBuf>, root: &Path, account: &str, label: &str) {
    let Some(codepoint) = qq_face_emoji_codepoint(label.trim()) else {
        return;
    };
    for db in ["EmojiFace.db", "SmallEmoji.db"] {
        push_with_thumbnail(
            out,
            root.join("qq-analyzer")
                .join("output")
                .join(account)
                .join("extracted-cfb")
                .join(db)
                .join("emoji")
                .join(format!("{codepoint}.png")),
        );
        push_with_thumbnail(
            out,
            root.join("qq-analyzer")
                .join("output")
                .join(account)
                .join("extracted-cfb-current")
                .join(db)
                .join("emoji")
                .join(format!("{codepoint}.png")),
        );
    }
}

fn qq_face_emoji_codepoint(label: &str) -> Option<&'static str> {
    match label {
        "[爱心]" => Some("0x2764"),
        _ => None,
    }
}

fn push_group2_image_candidates(
    out: &mut Vec<PathBuf>,
    root: &Path,
    account: &str,
    qq_group_rel: &str,
) {
    let Some(name) = qq_group_rel
        .rsplit('\\')
        .find(|part| !part.trim().is_empty())
        .map(str::trim)
    else {
        return;
    };
    let mut chars = name.chars();
    let first: String = chars.by_ref().take(2).collect();
    let second: String = chars.by_ref().take(2).collect();
    if first.chars().count() != 2 || second.chars().count() != 2 {
        return;
    }
    push_with_thumbnail(
        out,
        root.join(account)
            .join("Image")
            .join("Group2")
            .join(first)
            .join(second)
            .join(name),
    );
}

pub(crate) fn thumbnail_asset_candidates(root: &Path, account: &str, text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !(text.starts_with("UserDataImage:") || text.starts_with("UserDataCustomFace:")) {
        return out;
    }
    let normalized = text.trim().replace('/', "\\");
    let Some(name) = normalized
        .rsplit('\\')
        .find(|part| !part.trim().is_empty())
        .map(str::trim)
    else {
        return out;
    };
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    if stem.is_empty() {
        return out;
    }
    let base = root
        .join("qq-analyzer")
        .join("output")
        .join(account)
        .join("extracted-cfb")
        .join("Thumbnails.db")
        .join("PicFile");
    push_unique_path(&mut out, base.join(stem));
    push_unique_path(&mut out, base.join(name));
    out
}

fn push_file_recv_subdir_candidates(
    out: &mut Vec<PathBuf>,
    root: &Path,
    account: &str,
    name: &str,
) {
    let base = root.join(account).join("FileRecv");
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten().take(512) {
            if entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
                out.push(entry.path().join(name));
            }
        }
    }
}

pub(crate) fn push_unique_path(out: &mut Vec<PathBuf>, path: PathBuf) {
    if !out.contains(&path) {
        out.push(path);
    }
}

fn path_from_qq_rel(text: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in text.split('\\') {
        let part = part.trim();
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        out.push(part);
    }
    out
}

fn push_with_thumbnail(out: &mut Vec<PathBuf>, path: PathBuf) {
    push_unique_path(out, path.clone());
    if let (Some(stem), Some(ext), Some(parent)) = (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|s| s.to_str()),
        path.parent(),
    ) {
        let mut exts = vec![ext.to_string()];
        if matches!(
            ext.to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp"
        ) {
            for alt in ["jpg", "jpeg", "png", "gif", "bmp", "webp"] {
                if !exts.iter().any(|value| value.eq_ignore_ascii_case(alt)) {
                    exts.push(alt.to_string());
                }
            }
        }
        for candidate_ext in exts {
            push_unique_path(out, parent.join(format!("{stem}_tmb.{candidate_ext}")));
            push_unique_path(out, parent.join(format!("{stem}fix.{candidate_ext}")));
        }
    }
}

fn looks_like_filename(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".amr", ".mp3", ".wav", ".ogg", ".m4a",
        ".mp4", ".webm", ".mov", ".zip", ".rar", ".7z", ".pdf", ".doc", ".docx",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}

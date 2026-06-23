use std::env;
use std::path::PathBuf;

use serde_json::json;

use msg3_richtext_parser_rs::config;
use msg3_richtext_parser_rs::info_storage::InfoStorage;
use msg3_richtext_parser_rs::qq_hash::{decode_hash, decode_hex_16, encode_hash};

fn main() -> anyhow::Result<()> {
    let mut root = config::default_root()?;
    let mut account = None::<String>;
    let mut group = String::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().unwrap_or_default()),
            "--account" => account = Some(args.next().unwrap_or_default()),
            "--group" => group = args.next().unwrap_or_default(),
            _ => {}
        }
    }
    if group.is_empty() {
        anyhow::bail!("usage: group_avatar_probe --root <workspace> --group <group_id>");
    }
    let account = config::resolve_account(&root, account)?;
    let info = InfoStorage::new(
        config::info_root_current(&root, &account),
        config::live_info_key_log(&root, &account),
    );
    let profile = info.group_profile(&group)?;
    let encoded_from_buf_file_hash = decode_hex_16(&profile.file_hash_hex)
        .and_then(|hash| encode_hash(&hash))
        .unwrap_or_default();
    let basename = path_basename(&profile.image_file_path).unwrap_or_default();
    let stem = basename.rsplit_once('.').map(|v| v.0).unwrap_or(&basename);
    let decoded_path_stem = decode_hash(stem)
        .map(|bytes| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .unwrap_or_default();
    let mut encoded_names = vec![encoded_from_buf_file_hash.clone()];
    if let Some(ext) = basename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
    {
        encoded_names.push(format!("{encoded_from_buf_file_hash}.{ext}"));
    }
    for ext in ["bmp", "gif", "jpg", "png"] {
        encoded_names.push(format!("{encoded_from_buf_file_hash}.{ext}"));
    }
    encoded_names.sort();
    encoded_names.dedup();
    let mut candidates = vec![];
    let mut names = vec![basename.clone()];
    names.extend(encoded_names.clone());
    for name in names {
        if name.is_empty() {
            continue;
        }
        for path in [
            root.join(&account).join("GroupCustomHeadImage").join(&name),
            root.join(&account)
                .join("Misc")
                .join("GroupCustomHeadImage")
                .join(&name),
            root.join("qq-analyzer")
                .join("output")
                .join(&account)
                .join("extracted-cfb")
                .join("MiscHead.db")
                .join(&name),
            root.join("qq-analyzer")
                .join("output")
                .join(&account)
                .join("extracted-cfb-current")
                .join("MiscHead.db")
                .join(&name),
        ] {
            candidates.push(json!({
                "path": path.to_string_lossy(),
                "exists": path.exists(),
                "file": path.is_file(),
            }));
        }
    }
    let sel = root
        .join(&account)
        .join("SelCreateGroupHead")
        .join(format!("{group}g"));
    candidates.push(json!({
        "path": sel.to_string_lossy(),
        "exists": sel.exists(),
        "file": sel.is_file(),
    }));
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "group": group,
            "account": account,
            "profile": profile,
            "basename": basename,
            "basename_stem_decoded_hash_hex": decoded_path_stem,
            "encoded_from_bufFileHash": encoded_from_buf_file_hash,
            "encoded_from_bufFileHash_names": encoded_names,
            "candidates": candidates,
        }))?
    );
    Ok(())
}

fn path_basename(value: &str) -> Option<String> {
    let normalized = value.trim().replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
}

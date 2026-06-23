use std::collections::HashSet;
use std::env;
use std::path::PathBuf;

use anyhow::Context;
use msg3_richtext_parser_rs::config;
use msg3_richtext_parser_rs::info_storage::InfoStorage;
use serde::Serialize;

#[derive(Serialize)]
struct FieldOut {
    index: usize,
    header: u8,
    name: String,
    value_len: u32,
    uint_value: Option<u32>,
    texts: Vec<String>,
    value_hex_prefix: String,
}

fn uint_value(header: u8, value: &[u8]) -> Option<u32> {
    match (header, value) {
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

fn main() -> anyhow::Result<()> {
    let mut workspace_root = config::default_root()?;
    let mut account = None::<String>;
    let mut root = None::<PathBuf>;
    let mut key_log = None::<PathBuf>;
    let mut stream = String::new();
    let mut uins: HashSet<String> = HashSet::new();

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace-root" => {
                workspace_root = PathBuf::from(args.next().context("--workspace-root needs value")?)
            }
            "--account" => account = Some(args.next().context("--account needs value")?),
            "--root" => root = Some(PathBuf::from(args.next().context("--root needs value")?)),
            "--key-log" => {
                key_log = Some(PathBuf::from(args.next().context("--key-log needs value")?))
            }
            "--stream" => stream = args.next().context("--stream needs value")?,
            "--uin" => {
                uins.insert(args.next().context("--uin needs value")?);
            }
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }

    if stream.is_empty() {
        anyhow::bail!("--stream is required");
    }

    let account = config::resolve_account(&workspace_root, account)?;
    let root = root.unwrap_or_else(|| {
        config::account_output_root(&workspace_root, &account)
            .join("extracted-cfb")
            .join("Info.db")
    });
    let key_log = key_log.unwrap_or_else(|| config::live_info_key_log(&workspace_root, &account));
    let storage = InfoStorage::new(root, key_log);
    let entries =
        storage.entries_for_stream(&stream, if uins.is_empty() { None } else { Some(&uins) })?;
    let mut json = serde_json::Map::new();
    for (key, fields) in entries {
        let field_out = fields
            .iter()
            .enumerate()
            .map(|(index, field)| FieldOut {
                index,
                header: field.header,
                name: field.name.clone(),
                value_len: field.value_len,
                uint_value: uint_value(field.header, &field.value),
                texts: field.value_texts.clone(),
                value_hex_prefix: hex_prefix(&field.value, 80),
            })
            .collect::<Vec<_>>();
        json.insert(key, serde_json::to_value(field_out)?);
    }

    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

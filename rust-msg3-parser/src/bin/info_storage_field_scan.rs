use std::collections::HashSet;
use std::env;
use std::path::PathBuf;

use serde_json::json;

use msg3_richtext_parser_rs::config;
use msg3_richtext_parser_rs::info_storage::InfoStorage;

fn main() -> anyhow::Result<()> {
    let mut workspace_root = config::default_root()?;
    let mut account = None::<String>;
    let mut root = None::<PathBuf>;
    let mut key_log = None::<PathBuf>;
    let mut stream = String::new();
    let mut fields = HashSet::new();
    let mut uins = HashSet::new();
    let mut limit = 50usize;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace-root" => workspace_root = PathBuf::from(args.next().unwrap_or_default()),
            "--account" => account = Some(args.next().unwrap_or_default()),
            "--root" => root = Some(PathBuf::from(args.next().unwrap_or_default())),
            "--key-log" => key_log = Some(PathBuf::from(args.next().unwrap_or_default())),
            "--stream" => stream = args.next().unwrap_or_default(),
            "--field" => {
                fields.insert(args.next().unwrap_or_default());
            }
            "--uin" => {
                uins.insert(args.next().unwrap_or_default().parse::<u32>()?);
            }
            "--limit" => limit = args.next().unwrap_or_default().parse()?,
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }
    if stream.is_empty() {
        anyhow::bail!("--stream is required");
    }
    let account = config::resolve_account(&workspace_root, account)?;
    let root = root.unwrap_or_else(|| config::info_root_current(&workspace_root, &account));
    let key_log = key_log.unwrap_or_else(|| config::live_info_key_log(&workspace_root, &account));
    let storage = InfoStorage::new(root, key_log);
    let entries = storage.entries_for_stream(&stream, None)?;
    let mut out = Vec::new();
    for (key, tx_fields) in entries {
        let has_field = !fields.is_empty() && tx_fields.iter().any(|f| fields.contains(&f.name));
        let has_uin = !uins.is_empty()
            && tx_fields.iter().any(|f| {
                if !matches!(f.header, 1 | 6) || f.value.len() != 4 {
                    return false;
                }
                uins.contains(&u32::from_le_bytes([
                    f.value[0], f.value[1], f.value[2], f.value[3],
                ]))
            });
        if !has_field && !has_uin {
            continue;
        }
        let fields_json = tx_fields
            .iter()
            .filter(|f| fields.is_empty() || fields.contains(&f.name) || f.name == "dwUin")
            .map(|f| {
                let uint_value = if matches!(f.header, 1 | 6) && f.value.len() == 4 {
                    Some(u32::from_le_bytes([
                        f.value[0], f.value[1], f.value[2], f.value[3],
                    ]))
                } else {
                    None
                };
                json!({
                    "name": f.name,
                    "header": f.header,
                    "value_len": f.value_len,
                    "uint_value": uint_value,
                    "texts": f.value_texts,
                    "hex": f.value.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                })
            })
            .collect::<Vec<_>>();
        out.push(json!({"key": key, "fields": fields_json}));
        if out.len() >= limit {
            break;
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"stream": stream, "matches": out}))?
    );
    Ok(())
}

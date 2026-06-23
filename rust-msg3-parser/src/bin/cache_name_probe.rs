use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;

use serde_json::json;

use msg3_richtext_parser_rs::qq_hash::decode_hash;

fn main() -> anyhow::Result<()> {
    let mut dir = PathBuf::new();
    let mut targets = HashSet::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = PathBuf::from(args.next().unwrap_or_default()),
            "--target" => {
                if let Some(value) = args.next() {
                    targets.insert(value.to_ascii_lowercase());
                }
            }
            _ => {}
        }
    }
    if dir.as_os_str().is_empty() {
        anyhow::bail!("usage: cache_name_probe --dir <cache_dir> [--target <hex16> ...]");
    }
    let mut scanned = 0usize;
    let mut decodable = 0usize;
    let mut matches = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        scanned += 1;
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(decoded) = decode_hash(stem) else {
            continue;
        };
        decodable += 1;
        let hex = decoded
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if targets.is_empty() || targets.contains(&hex) {
            matches.push(json!({
                "path": path.to_string_lossy(),
                "name": path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                "decoded_hash_hex": hex,
            }));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "dir": dir.to_string_lossy(),
            "scanned_files": scanned,
            "decodable_23_char_stems": decodable,
            "matches": matches,
        }))?
    );
    Ok(())
}

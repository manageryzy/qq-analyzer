use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::config;

#[derive(Clone, Debug, Serialize)]
pub struct InventoryItem {
    pub role: String,
    pub path: String,
    pub exists: bool,
    pub file: bool,
    pub dir: bool,
    pub size: u64,
    pub modified_unix: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct InventoryReport {
    pub root: String,
    pub account: String,
    pub roots: Vec<InventoryItem>,
    pub databases: Vec<DatabaseItem>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DatabaseItem {
    pub path: String,
    pub account_relative: String,
    pub source_class: String,
    pub header_kind: String,
    pub size: u64,
    pub modified_unix: i64,
}

pub fn build_inventory(root: &Path, account: &str) -> InventoryReport {
    let output = config::account_output_root(root, account);
    let account_root = root.join(account);
    let roots = vec![
        item("account_root", account_root.clone()),
        item("msg3_db_prepared", config::msg3_db(root, account)),
        item(
            "msg3_index_db_prepared",
            config::msg3_index_db(root, account),
        ),
        item("legacy_msg3_db", config::legacy_msg3_db(root, account)),
        item(
            "legacy_msg3_index_db",
            output.join("pcqq_live").join("db").join("Msg3.0index.db"),
        ),
        item(
            "infostorage_current",
            config::info_root_current(root, account),
        ),
        item(
            "infostorage_legacy_current",
            config::legacy_info_root_current(root, account),
        ),
        item(
            "infostorage_key_log",
            config::live_info_key_log(root, account),
        ),
        item(
            "credentials",
            output.join("credentials").join("credentials.jsonl"),
        ),
        item("prepared_cfb", config::prepared_cfb_root(root, account)),
        item("prepared_root", output.join("prepared")),
        item("catalog_root", output.join("catalog")),
        item("image_root", root.join(account).join("Image")),
        item("file_recv_root", root.join(account).join("FileRecv")),
        item("custom_face_root", root.join(account).join("CustomFace")),
        item(
            "extracted_cfb_current",
            output.join("extracted-cfb-current"),
        ),
        item("extracted_cfb", output.join("extracted-cfb")),
    ];
    InventoryReport {
        root: root.display().to_string(),
        account: account.to_string(),
        roots,
        databases: discover_databases(root, account),
    }
}

pub fn write_reports(dir: &Path, report: &InventoryReport) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    write_jsonl(&dir.join("roots.jsonl"), &report.roots)?;
    write_jsonl(&dir.join("databases.jsonl"), &report.databases)?;
    Ok(())
}

pub fn write_manifest(path: &Path, report: &InventoryReport) -> anyhow::Result<()> {
    write_jsonl(path, &report.roots)
}

fn item(role: &str, path: PathBuf) -> InventoryItem {
    let meta = fs::metadata(&path).ok();
    let modified_unix = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    InventoryItem {
        role: role.to_string(),
        path: path.display().to_string(),
        exists: meta.is_some(),
        file: meta.as_ref().map(|m| m.is_file()).unwrap_or(false),
        dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
        size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
        modified_unix,
    }
}

fn discover_databases(root: &Path, account: &str) -> Vec<DatabaseItem> {
    let account_root = root.join(account);
    let dirs = [
        (account_root.clone(), "pcqq"),
        (account_root.join("QQ"), "pcqq"),
        (account_root.join("Audio").join("AudioInfo"), "pcqq"),
        (account_root.join("MyCollection"), "pcqq"),
        (account_root.join("GroupCollection"), "pcqq"),
        (account_root.join("nt_qq").join("nt_db"), "ntqq"),
        (account_root.join("nt_qq").join("nt_data"), "ntqq"),
        (
            root.join("nt_qq").join("global").join("nt_db"),
            "ntqq_global",
        ),
    ];
    let mut out = Vec::new();
    for (dir, source_class) in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("db") {
                continue;
            }
            let meta = match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => meta,
                _ => continue,
            };
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let account_relative = path
                .strip_prefix(&account_root)
                .or_else(|_| path.strip_prefix(root))
                .unwrap_or(&path)
                .display()
                .to_string();
            out.push(DatabaseItem {
                path: path.display().to_string(),
                account_relative,
                source_class: source_class.to_string(),
                header_kind: header_kind(&path),
                size: meta.len(),
                modified_unix,
            });
        }
    }
    out.sort_by(|a, b| a.account_relative.cmp(&b.account_relative));
    out
}

fn header_kind(path: &Path) -> String {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return "unreadable".to_string(),
    };
    let mut head = [0u8; 16];
    let Ok(read) = file.read(&mut head) else {
        return "unreadable".to_string();
    };
    let head = &head[..read];
    if head == b"SQLite header 3\0" {
        "pcqq_encrypted_sqlite".to_string()
    } else if head == b"SQLite format 3\0" {
        "sqlite".to_string()
    } else if head.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
        "ole_compound".to_string()
    } else {
        head.iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut lines = String::new();
    for row in rows {
        lines.push_str(&serde_json::to_string(row)?);
        lines.push('\n');
    }
    fs::write(path, lines)?;
    Ok(())
}

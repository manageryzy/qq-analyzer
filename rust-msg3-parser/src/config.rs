use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub fn default_root() -> anyhow::Result<PathBuf> {
    if let Ok(root) = env::var("QQ_ANALYZER_ROOT") {
        if !root.trim().is_empty() {
            return Ok(PathBuf::from(root));
        }
    }
    Ok(env::current_dir()?)
}

pub fn resolve_account(root: &Path, account: Option<String>) -> anyhow::Result<String> {
    if let Some(account) = account.filter(|value| !value.trim().is_empty()) {
        return Ok(account);
    }
    if let Ok(account) = env::var("QQ_ANALYZER_ACCOUNT") {
        if !account.trim().is_empty() {
            return Ok(account);
        }
    }
    discover_account(root)
}

pub fn account_output_root(root: &Path, account: &str) -> PathBuf {
    root.join("qq-analyzer").join("output").join(account)
}

pub fn msg3_db(root: &Path, account: &str) -> PathBuf {
    prepared_pcqq_db_root(root, account).join("Msg3.0.db")
}

pub fn msg3_index_db(root: &Path, account: &str) -> PathBuf {
    prepared_pcqq_db_root(root, account).join("Msg3.0index.db")
}

pub fn legacy_msg3_db(root: &Path, account: &str) -> PathBuf {
    account_output_root(root, account)
        .join("pcqq_live")
        .join("db")
        .join("Msg3.0.db")
}

pub fn prepared_pcqq_db_root(root: &Path, account: &str) -> PathBuf {
    account_output_root(root, account)
        .join("prepared")
        .join("pcqq")
        .join("db")
}

pub fn prepared_cfb_root(root: &Path, account: &str) -> PathBuf {
    account_output_root(root, account)
        .join("prepared")
        .join("pcqq")
        .join("cfb")
}

pub fn info_root_current(root: &Path, account: &str) -> PathBuf {
    prepared_cfb_root(root, account).join("Info.db")
}

pub fn legacy_info_root_current(root: &Path, account: &str) -> PathBuf {
    account_output_root(root, account)
        .join("extracted-cfb-current")
        .join("Info.db")
}

pub fn live_info_key_log(root: &Path, account: &str) -> PathBuf {
    account_output_root(root, account)
        .join("pcqq_live")
        .join("infostorage_keys.jsonl")
}

pub fn discover_account(root: &Path) -> anyhow::Result<String> {
    let output = root.join("qq-analyzer").join("output");
    let mut accounts = Vec::new();
    if let Ok(entries) = fs::read_dir(&output) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
                continue;
            }
            let account = entry.file_name().to_string_lossy().to_string();
            if entry
                .path()
                .join("pcqq_live")
                .join("db")
                .join("Msg3.0.db")
                .is_file()
            {
                accounts.push(account);
            }
        }
    }
    accounts.sort();
    accounts.dedup();
    match accounts.as_slice() {
        [account] => Ok(account.clone()),
        [] => anyhow::bail!(
            "no account with pcqq_live/db/Msg3.0.db found under {}; pass --account or set QQ_ANALYZER_ACCOUNT",
            output.display()
        ),
        many => anyhow::bail!(
            "multiple accounts found under {}: {}; pass --account or set QQ_ANALYZER_ACCOUNT",
            output.display(),
            many.join(", ")
        ),
    }
}

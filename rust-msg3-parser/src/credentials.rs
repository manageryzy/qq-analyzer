use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    PcqqSqliteKey,
    InfostorageTeaKey,
    NtqqSqlcipherKey,
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CredentialRecord {
    pub kind: CredentialKind,
    pub account: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub db_path: String,
    pub key_hex: String,
    #[serde(default)]
    pub key_len: usize,
    #[serde(default)]
    pub captured_at: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl CredentialRecord {
    pub fn normalized(mut self) -> Self {
        self.key_hex = clean_hex(&self.key_hex);
        if self.key_len == 0 {
            self.key_len = self.key_hex.len() / 2;
        }
        if self.captured_at.is_empty() {
            self.captured_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        }
        self
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.account.trim().is_empty() {
            anyhow::bail!("credential account is empty");
        }
        if self.key_hex.is_empty() || self.key_hex.len() % 2 != 0 {
            anyhow::bail!("credential key_hex must be non-empty even-length hex");
        }
        if !self.key_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("credential key_hex contains non-hex characters");
        }
        if self.key_len != self.key_hex.len() / 2 {
            anyhow::bail!(
                "credential key_len={} does not match key_hex bytes={}",
                self.key_len,
                self.key_hex.len() / 2
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CredentialSummary {
    pub path: String,
    pub records: usize,
    pub pcqq_sqlite_keys: usize,
    pub infostorage_tea_keys: usize,
    pub ntqq_sqlcipher_keys: usize,
}

pub fn default_credentials_path(root: &Path, account: &str) -> PathBuf {
    config::account_output_root(root, account)
        .join("credentials")
        .join("credentials.jsonl")
}

pub fn legacy_infostorage_key_path(root: &Path, account: &str) -> PathBuf {
    config::live_info_key_log(root, account)
}

pub fn read_credentials(path: &Path) -> anyhow::Result<Vec<CredentialRecord>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: CredentialRecord = serde_json::from_str(trimmed)
            .map_err(|err| anyhow::anyhow!("{}:{}: {err}", path.display(), index + 1))?;
        let record = record.normalized();
        record.validate()?;
        records.push(record);
    }
    Ok(records)
}

pub fn append_credential(path: &Path, record: CredentialRecord) -> anyhow::Result<()> {
    let record = record.normalized();
    record.validate()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(&record)?)?;
    Ok(())
}

pub fn summarize(path: &Path) -> anyhow::Result<CredentialSummary> {
    let records = if path.is_file() {
        read_credentials(path)?
    } else {
        Vec::new()
    };
    let mut summary = CredentialSummary {
        path: path.display().to_string(),
        records: records.len(),
        ..CredentialSummary::default()
    };
    for record in records {
        match record.kind {
            CredentialKind::PcqqSqliteKey => summary.pcqq_sqlite_keys += 1,
            CredentialKind::InfostorageTeaKey => summary.infostorage_tea_keys += 1,
            CredentialKind::NtqqSqlcipherKey => summary.ntqq_sqlcipher_keys += 1,
            CredentialKind::Other => {}
        }
    }
    Ok(summary)
}

pub fn has_kind_for_account(
    records: &[CredentialRecord],
    account: &str,
    kind: CredentialKind,
) -> bool {
    records
        .iter()
        .any(|record| record.account == account && record.kind == kind)
}

pub fn pcqq_sqlite_key_for_path(
    records: &[CredentialRecord],
    path: &Path,
) -> Option<CredentialRecord> {
    key_for_path(records, path, CredentialKind::PcqqSqliteKey)
}

pub fn ntqq_sqlcipher_key_for_path(
    records: &[CredentialRecord],
    path: &Path,
) -> Option<CredentialRecord> {
    key_for_path(records, path, CredentialKind::NtqqSqlcipherKey)
}

fn key_for_path(
    records: &[CredentialRecord],
    path: &Path,
    kind: CredentialKind,
) -> Option<CredentialRecord> {
    let path_text = path.display().to_string();
    let path_norm = normalize_path_text(&path_text);
    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    records
        .iter()
        .filter(|record| record.kind == kind)
        .find(|record| normalize_path_text(&record.db_path) == path_norm)
        .cloned()
        .or_else(|| {
            records
                .iter()
                .filter(|record| record.kind == kind)
                .find(|record| {
                    !basename.is_empty()
                        && record
                            .metadata
                            .get("basename")
                            .map(|value| value.eq_ignore_ascii_case(&basename))
                            .unwrap_or(false)
                })
                .cloned()
        })
        .or_else(|| {
            records
                .iter()
                .filter(|record| record.kind == kind && record.key_len > 0)
                .next()
                .cloned()
        })
}

fn clean_hex(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !matches!(ch, ' ' | ':' | '-' | '\t' | '\r' | '\n'))
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn normalize_path_text(value: &str) -> String {
    value
        .trim()
        .replace('/', "\\")
        .to_ascii_lowercase()
        .trim_start_matches("\\\\?\\")
        .to_string()
}

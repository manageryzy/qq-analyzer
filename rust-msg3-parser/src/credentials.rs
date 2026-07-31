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
        if self.key_hex.is_empty() || !self.key_hex.len().is_multiple_of(2) {
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
    let exact_matches = records
        .iter()
        .filter(|record| record.kind == kind && normalize_path_text(&record.db_path) == path_norm)
        .collect::<Vec<_>>();
    if !exact_matches.is_empty() {
        return unambiguous_key(exact_matches);
    }

    let basename_matches = records
        .iter()
        .filter(|record| {
            record.kind == kind
                && !basename.is_empty()
                && record
                    .metadata
                    .get("basename")
                    .is_some_and(|value| value.eq_ignore_ascii_case(&basename))
        })
        .collect::<Vec<_>>();
    if !basename_matches.is_empty() {
        return unambiguous_key(basename_matches);
    }

    unambiguous_key(records.iter().filter(|record| record.kind == kind))
}

fn unambiguous_key<'a>(
    records: impl IntoIterator<Item = &'a CredentialRecord>,
) -> Option<CredentialRecord> {
    let mut records = records.into_iter().filter(|record| record.key_len > 0);
    let candidate = records.next()?;
    records
        .all(|record| record.key_hex.eq_ignore_ascii_case(&candidate.key_hex))
        .then(|| candidate.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, key_hex: &str) -> CredentialRecord {
        CredentialRecord {
            kind: CredentialKind::PcqqSqliteKey,
            account: "account".to_string(),
            source: "test".to_string(),
            db_path: path.to_string(),
            key_hex: key_hex.to_string(),
            key_len: key_hex.len() / 2,
            captured_at: String::new(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn key_lookup_rejects_ambiguous_fallback() {
        let records = vec![record("a.db", "0011"), record("b.db", "2233")];

        assert!(pcqq_sqlite_key_for_path(&records, Path::new("unknown.db")).is_none());
        assert_eq!(
            pcqq_sqlite_key_for_path(&records, Path::new("a.db"))
                .unwrap()
                .key_hex,
            "0011"
        );
    }

    #[test]
    fn key_lookup_allows_single_fallback() {
        let records = vec![record("a.db", "0011")];

        assert_eq!(
            pcqq_sqlite_key_for_path(&records, Path::new("unknown.db"))
                .unwrap()
                .key_hex,
            "0011"
        );
    }

    #[test]
    fn key_lookup_rejects_conflicting_basename_matches() {
        let mut first = record("account-a/msg.db", "0011");
        first
            .metadata
            .insert("basename".to_string(), "msg.db".to_string());
        let mut second = record("account-b/msg.db", "2233");
        second
            .metadata
            .insert("basename".to_string(), "msg.db".to_string());

        assert!(pcqq_sqlite_key_for_path(&[first, second], Path::new("msg.db")).is_none());
    }

    #[test]
    fn key_lookup_accepts_repeated_capture_of_same_key() {
        let records = vec![record("a.db", "0011"), record("a.db", "0011")];

        assert_eq!(
            pcqq_sqlite_key_for_path(&records, Path::new("a.db"))
                .unwrap()
                .key_hex,
            "0011"
        );
    }
}

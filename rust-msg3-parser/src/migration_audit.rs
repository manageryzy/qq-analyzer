use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct PythonAuditReport {
    pub root: String,
    pub analyzer_dir: String,
    pub files: Vec<PythonAuditFile>,
    pub totals: PythonAuditTotals,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PythonAuditTotals {
    pub files: usize,
    pub archived_python: usize,
    pub rust_replaced: usize,
    pub todo_ntqq: usize,
    pub legacy_probe: usize,
    pub unknown_python: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct PythonAuditFile {
    pub path: String,
    pub name: String,
    pub status: String,
    pub replacement: String,
    pub note: String,
}

pub fn audit_python(root: &Path) -> anyhow::Result<PythonAuditReport> {
    let analyzer_dir = analyzer_dir(root);
    let mut files = Vec::new();
    if analyzer_dir.is_dir() {
        collect_python_files(&analyzer_dir, &mut files)?;
    }
    files.sort();
    let audited = files
        .into_iter()
        .map(|path| audit_file(&analyzer_dir, path))
        .collect::<Vec<_>>();
    let mut totals = PythonAuditTotals {
        files: audited.len(),
        archived_python: count_archived_python(&analyzer_dir)?,
        ..PythonAuditTotals::default()
    };
    for file in &audited {
        match file.status.as_str() {
            "rust_replaced" => totals.rust_replaced += 1,
            "todo_ntqq" => totals.todo_ntqq += 1,
            "legacy_probe" => totals.legacy_probe += 1,
            "unknown_python" => totals.unknown_python += 1,
            _ => {}
        }
    }
    Ok(PythonAuditReport {
        root: root.display().to_string(),
        analyzer_dir: analyzer_dir.display().to_string(),
        files: audited,
        totals,
    })
}

fn analyzer_dir(root: &Path) -> PathBuf {
    let nested = root.join("qq-analyzer");
    if nested.is_dir() {
        nested
    } else {
        root.to_path_buf()
    }
}

fn collect_python_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || matches!(name.as_str(), ".venv" | "__pycache__") {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if matches!(name.as_str(), "archive" | "output" | "target") {
                continue;
            }
            collect_python_files(&path, files)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("py") {
            files.push(path);
        }
    }
    Ok(())
}

fn count_archived_python(analyzer_dir: &Path) -> anyhow::Result<usize> {
    let archive = analyzer_dir.join("archive").join("python-legacy");
    if !archive.is_dir() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in fs::read_dir(archive)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("py") {
            count += 1;
        }
    }
    Ok(count)
}

fn audit_file(analyzer_dir: &Path, path: PathBuf) -> PythonAuditFile {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let (status, replacement, note) = classify_python(&name);
    PythonAuditFile {
        path: path
            .strip_prefix(analyzer_dir)
            .unwrap_or(&path)
            .display()
            .to_string(),
        name,
        status: status.to_string(),
        replacement: replacement.to_string(),
        note: note.to_string(),
    }
}

fn classify_python(name: &str) -> (&'static str, &'static str, &'static str) {
    match name {
        "analyze_pcqq_databases.py" => (
            "rust_replaced",
            "qq_analyzer_rs db analyze",
            "schema/header Markdown analysis is Rust-owned",
        ),
        "check_html_dead_links.py" => (
            "rust_replaced",
            "qq_analyzer_rs html check-links",
            "generated HTML link checks are Rust-owned",
        ),
        "dump_sqlite_sample.py" => (
            "rust_replaced",
            "qq_analyzer_rs db sample",
            "read-only SQLite sampling is Rust-owned",
        ),
        "find_sender_rows.py" => (
            "rust_replaced",
            "qq_analyzer_rs db sender-rows",
            "SenderUin lookup is Rust-owned",
        ),
        "export_richtext_samples.py" => (
            "rust_replaced",
            "qq_analyzer_rs msg3 export-samples",
            "Msg3 parser sample export is Rust-owned",
        ),
        "query_msgindex_group.py" => (
            "rust_replaced",
            "qq_analyzer_rs msg3 index-query",
            "MsgIndex lookup is Rust-owned",
        ),
        "pcqq_info_storage.py" => (
            "rust_replaced",
            "qq_analyzer_rs info ...",
            "common InfoStorage label/profile/stream inspection is Rust-owned",
        ),
        "find_msg3_image_asset_names.py" => (
            "rust_replaced",
            "qq_analyzer_rs assets basename-match",
            "explicit-root image asset matching is Rust-owned",
        ),
        "check_c2c_md5_hits.py" => (
            "rust_replaced",
            "qq_analyzer_rs assets c2c-md5-hits",
            "C2C MD5 hit checks are Rust-owned",
        ),
        "probe_image_candidate_rules.py" => (
            "rust_replaced",
            "qq_analyzer_rs assets candidate-rules",
            "asset candidate rule probes are Rust-owned",
        ),
        "extract_pcqq_cfb.py" | "qq_cfb_reader.py" => (
            "rust_replaced",
            "qq_analyzer_rs preprocess --extract-cfb",
            "CFB extraction is Rust-owned",
        ),
        "msg3_log_service.py" | "restore_msg3_html.py" | "extract_msg3_messages.py" => (
            "rust_replaced",
            "qq_analyzer_rs serve / msg3 ...",
            "Msg3 service, parsing, and export workflows are Rust-owned",
        ),
        "capture_pcqq_infostorage_key.py" | "pcqq_live_dump_all.py" => (
            "rust_replaced",
            "qq_analyzer_rs capture ... / preprocess ...",
            "Frida attach orchestration and PCQQ preprocessing are Rust-owned",
        ),
        "qq_analyzer.py" | "ntqq_key_discover.py" => (
            "todo_ntqq",
            "future Rust NTQQ SQLCipher decrypt/export",
            "NTQQ decrypt/export is intentionally deferred",
        ),
        name if name.starts_with("decrypt_") => (
            "todo_ntqq",
            "future Rust NTQQ SQLCipher decrypt/export",
            "account-specific decrypt helpers are local-only legacy files",
        ),
        name if name.starts_with("inspect_")
            || name.starts_with("probe_")
            || name.starts_with("audit_")
            || name.starts_with("verify_")
            || name.starts_with("compare_")
            || name.starts_with("scan_")
            || name.starts_with("find_infostorage_")
            || name.starts_with("pcqq_decrypt_")
            || matches!(
                name,
                "pcqq_legacy_container.py"
                    | "pcqq_td_records.py"
                    | "extract_qq_rdb_resource.py"
                    | "msg3_proto_probe.py"
            ) =>
        {
            (
                "legacy_probe",
                "none",
                "historical one-off probe; migrate only if durable behavior is needed",
            )
        }
        _ => (
            "unknown_python",
            "none",
            "unclassified Python file; decide whether to migrate, archive, or mark legacy",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ntqq_as_todo() {
        let (status, replacement, _) = classify_python("qq_analyzer.py");
        assert_eq!(status, "todo_ntqq");
        assert!(replacement.contains("NTQQ"));
    }

    #[test]
    fn classifies_known_replacement() {
        let (status, replacement, _) = classify_python("dump_sqlite_sample.py");
        assert_eq!(status, "rust_replaced");
        assert_eq!(replacement, "qq_analyzer_rs db sample");
    }
}

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use std::time::Duration;

use crate::{capture, catalog, cfb, config, credentials, inventory, snapshot};

#[derive(Clone, Debug, Serialize)]
pub struct PreprocessStep {
    pub name: String,
    pub ok: bool,
    pub input: String,
    pub output: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PreprocessReport {
    pub root: String,
    pub account: String,
    pub prepared_root: String,
    pub credentials: credentials::CredentialSummary,
    pub steps: Vec<PreprocessStep>,
}

#[derive(Clone, Debug, Default)]
pub struct PreprocessOptions {
    pub prepare_pcqq_dbs: bool,
    pub prepare_ntqq_dbs: bool,
    pub rekey_pcqq_dbs: bool,
    pub extract_cfb: bool,
    pub cfb_stream_limit: Option<usize>,
    pub force: bool,
    pub db_limit: Option<usize>,
    pub only: Vec<String>,
    pub frida: Option<String>,
    pub frida_version: Option<String>,
    pub process: Option<String>,
    pub pid: Option<u32>,
    pub timeout_seconds: Option<u64>,
}

pub fn run_preprocess(
    root: &Path,
    account: &str,
    credential_path: &Path,
) -> anyhow::Result<PreprocessReport> {
    run_preprocess_with_options(
        root,
        account,
        credential_path,
        &PreprocessOptions::default(),
    )
}

pub fn run_preprocess_with_options(
    root: &Path,
    account: &str,
    credential_path: &Path,
    options: &PreprocessOptions,
) -> anyhow::Result<PreprocessReport> {
    let output = config::account_output_root(root, account);
    let prepared_root = output.join("prepared");
    let inventory_root = output.join("inventory");
    fs::create_dir_all(output.join("catalog"))?;
    fs::create_dir_all(&inventory_root)?;

    let inv = inventory::build_inventory(root, account);
    inventory::write_reports(&inventory_root, &inv)?;

    let credentials = credentials::summarize(credential_path)?;
    let mut steps = Vec::new();
    steps.push(register_path(
        "register_infostorage",
        &config::info_root_current(root, account),
        &prepared_root.join("infostorage"),
        true,
    ));
    let credential_records = if credential_path.is_file() {
        credentials::read_credentials(credential_path).unwrap_or_default()
    } else {
        Vec::new()
    };
    if options.extract_cfb
        && !credentials::has_kind_for_account(
            &credential_records,
            account,
            credentials::CredentialKind::InfostorageTeaKey,
        )
    {
        anyhow::bail!(
            "missing infostorage_tea_key credential for account {account}; run `qq_analyzer_rs capture all --account {account}` before `preprocess --extract-cfb`"
        );
    }
    steps.extend(preprocess_ntqq_dbs(
        root,
        account,
        &prepared_root,
        options,
        &credential_records,
    )?);
    steps.extend(preprocess_pcqq_dbs(
        root,
        account,
        &prepared_root,
        options,
        &credential_records,
    )?);

    let report = PreprocessReport {
        root: root.display().to_string(),
        account: account.to_string(),
        prepared_root: prepared_root.display().to_string(),
        credentials,
        steps,
    };
    fs::write(
        output.join("catalog").join("preprocess-report.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    catalog::write_preprocess_report(&catalog::default_catalog_path(root, account), &report)?;
    Ok(report)
}

fn preprocess_pcqq_dbs(
    root: &Path,
    account: &str,
    prepared_root: &Path,
    options: &PreprocessOptions,
    credential_records: &[credentials::CredentialRecord],
) -> anyhow::Result<Vec<PreprocessStep>> {
    let account_root = root.join(account);
    let mut sources = if options.only.is_empty() {
        discover_classic_dbs(root, account)?
    } else {
        let mut selected = Vec::new();
        for selector in &options.only {
            selected.push(source_from_selector(root, account, selector)?);
        }
        selected
    };
    if let Some(limit) = options.db_limit {
        sources.truncate(limit);
    }
    let mut steps = Vec::new();
    for source in sources {
        let rel = source.strip_prefix(&account_root).unwrap_or(&source);
        if is_nt_path(&account_root, &source) {
            steps.push(step(
                "pcqq_db_skip_nt_path",
                true,
                &source,
                &prepared_root.join("ntqq").join("clean").join(rel),
                "skipped by PCQQ preprocessing; handled by NTQQ preprocessing",
            ));
            continue;
        }
        let kind = header_kind(&source)?;
        let (name, output) = match kind {
            DbHeaderKind::Sqlite => (
                "pcqq_db_sqlite",
                prepared_root.join("pcqq").join("db").join(rel),
            ),
            DbHeaderKind::PcqqEncryptedSqlite => (
                "pcqq_db_encrypted_sqlite",
                prepared_root.join("pcqq").join("encrypted-tmp").join(rel),
            ),
            DbHeaderKind::OleCompound | DbHeaderKind::Unknown(_) => (
                "pcqq_db_raw",
                prepared_root.join("pcqq").join("raw").join(rel),
            ),
        };
        let step = if options.prepare_pcqq_dbs {
            prepare_one_pcqq_db(
                root,
                account,
                &source,
                &output,
                kind,
                options,
                credential_records,
            )
        } else {
            step(
                name,
                true,
                &source,
                &output,
                format!("planned; kind={}", kind.as_str()),
            )
        };
        steps.push(step);
        if options.extract_cfb && matches!(kind, DbHeaderKind::OleCompound) {
            steps.push(extract_cfb_step(
                &source,
                prepared_root,
                &account_root,
                options.force,
                options.cfb_stream_limit,
            ));
        }
    }
    Ok(steps)
}

fn preprocess_ntqq_dbs(
    root: &Path,
    account: &str,
    prepared_root: &Path,
    options: &PreprocessOptions,
    credential_records: &[credentials::CredentialRecord],
) -> anyhow::Result<Vec<PreprocessStep>> {
    let account_root = root.join(account);
    let mut sources = if options.only.is_empty() {
        discover_ntqq_dbs(root, account)?
    } else {
        let mut selected = Vec::new();
        for selector in &options.only {
            let source = source_from_selector(root, account, selector)?;
            if is_nt_path(&account_root, &source) || is_global_nt_path(root, &source) {
                selected.push(source);
            }
        }
        selected
    };
    if let Some(limit) = options.db_limit {
        sources.truncate(limit);
    }
    let mut steps = Vec::new();
    for source in sources {
        let clean_output = ntqq_clean_output(root, account, prepared_root, &source);
        let key_state = if credentials::ntqq_sqlcipher_key_for_path(credential_records, &source)
            .or_else(|| credentials::ntqq_sqlcipher_key_for_path(credential_records, &clean_output))
            .is_some()
        {
            "ntqq_sqlcipher_key_available"
        } else {
            "ntqq_sqlcipher_key_missing"
        };
        if options.prepare_ntqq_dbs {
            steps.push(prepare_one_ntqq_db(
                &source,
                &clean_output,
                options.force,
                key_state,
            ));
        } else {
            steps.push(step(
                "ntqq_db_prefixed_sqlcipher",
                true,
                &source,
                &clean_output,
                format!("planned clean-copy; {key_state}"),
            ));
        }
    }
    Ok(steps)
}

fn prepare_one_ntqq_db(
    source: &Path,
    clean_output: &Path,
    force: bool,
    key_state: &str,
) -> PreprocessStep {
    match strip_ntqq_prefix(source, clean_output, force) {
        Ok(()) => step(
            "ntqq_db_clean_copy",
            true,
            source,
            clean_output,
            format!("stripped 1024-byte NTQQ prefix; {key_state}; SQLCipher decrypt/export remains a credentialed follow-up"),
        ),
        Err(err) => step(
            "ntqq_db_clean_copy",
            false,
            source,
            clean_output,
            format!("{err}; {key_state}"),
        ),
    }
}

fn prepare_one_pcqq_db(
    root: &Path,
    account: &str,
    source: &Path,
    output: &Path,
    kind: DbHeaderKind,
    options: &PreprocessOptions,
    credential_records: &[credentials::CredentialRecord],
) -> PreprocessStep {
    match kind {
        DbHeaderKind::Sqlite => {
            match copy_file(source, output, options.force).and_then(|_| sqlite_summary(output)) {
                Ok(detail) => step("pcqq_db_sqlite", true, source, output, detail),
                Err(err) => step("pcqq_db_sqlite", false, source, output, err.to_string()),
            }
        }
        DbHeaderKind::PcqqEncryptedSqlite => {
            let plain_output = pcqq_plain_output_from_rekeyed(output);
            match strip_pcqq_header(source, &plain_output, options.force)
                .and_then(|_| sqlite_summary(&plain_output))
            {
                Ok(detail) => step(
                    prepared_db_role(source),
                    true,
                    source,
                    &plain_output,
                    detail,
                ),
                Err(strip_err) => {
                    if options.rekey_pcqq_dbs {
                        rekey_prepared_pcqq_db(
                            root,
                            account,
                            source,
                            output,
                            &plain_output,
                            options,
                            credential_records,
                            strip_err.to_string(),
                        )
                    } else {
                        let key_state =
                            if credentials::pcqq_sqlite_key_for_path(credential_records, source)
                                .is_some()
                            {
                                "pcqq_sqlite_key_available"
                            } else {
                                "pcqq_sqlite_key_missing"
                            };
                        step(
                            "pcqq_db_rekey_required",
                            false,
                            source,
                            &plain_output,
                            format!(
                                "encrypted DB requires rekey; {key_state}; no encrypted working copy created without --rekey-pcqq-dbs; strip status: {strip_err}"
                            ),
                        )
                    }
                }
            }
        }
        DbHeaderKind::OleCompound | DbHeaderKind::Unknown(_) => {
            match copy_file(source, output, options.force) {
                Ok(()) => step(
                    "pcqq_db_raw",
                    true,
                    source,
                    output,
                    format!("copied raw; kind={}", kind.as_str()),
                ),
                Err(err) => step("pcqq_db_raw", false, source, output, err.to_string()),
            }
        }
    }
}

fn rekey_prepared_pcqq_db(
    root: &Path,
    account: &str,
    source: &Path,
    rekeyed_output: &Path,
    plain_output: &Path,
    options: &PreprocessOptions,
    credential_records: &[credentials::CredentialRecord],
    strip_err: String,
) -> PreprocessStep {
    let Some(key) = credentials::pcqq_sqlite_key_for_path(credential_records, source)
        .or_else(|| credentials::pcqq_sqlite_key_for_path(credential_records, rekeyed_output))
    else {
        return step(
            "pcqq_db_rekey_missing_key",
            false,
            source,
            rekeyed_output,
            format!("copied encrypted DB but no pcqq_sqlite_key credential matched; strip status: {strip_err}"),
        );
    };
    let working_copy = match snapshot::create_working_copy(source, rekeyed_output, options.force) {
        Ok(copy) => copy,
        Err(err) => {
            return step(
                "pcqq_db_snapshot_failed",
                false,
                source,
                rekeyed_output,
                format!(
                    "could not create encrypted working copy for rekey; strip status: {strip_err}; {err}"
                ),
            )
        }
    };
    let output_root = config::account_output_root(root, account);
    let capture_root = output_root.join("capture");
    let script_path = capture_root.join(format!("pcqq-rekey-{}.js", safe_db_name(rekeyed_output)));
    let events_path = capture_root.join(format!(
        "pcqq-rekey-{}.events.jsonl",
        safe_db_name(rekeyed_output)
    ));
    let frida = match &options.frida {
        Some(path) => path.clone(),
        None => match capture::ensure_frida_inject(&capture::FridaDependencyOptions {
            cache_root: output_root.join("deps").join("frida"),
            version: options.frida_version.clone(),
            force: false,
        }) {
            Ok(summary) => summary.executable_path,
            Err(err) => {
                return step(
                    "pcqq_db_rekey_runner_unavailable",
                    false,
                    source,
                    rekeyed_output,
                    format!("copied encrypted DB but frida runner is unavailable: {err}"),
                )
            }
        },
    };
    let process = if options.pid.is_some() {
        options.process.clone()
    } else {
        options
            .process
            .clone()
            .or_else(|| Some("QQ.exe".to_string()))
    };
    let summary = match capture::run_pcqq_rekey(&capture::PcqqRekeyOptions {
        frida,
        process,
        pid: options.pid,
        input_path: rekeyed_output.to_path_buf(),
        key_hex: key.key_hex,
        script_path,
        events_path,
        timeout: options.timeout_seconds.map(Duration::from_secs),
    }) {
        Ok(summary) => summary,
        Err(err) => {
            let _ = snapshot::remove_working_copy(rekeyed_output);
            return step(
                "pcqq_db_rekey_failed",
                false,
                source,
                rekeyed_output,
                format!("Rust rekey orchestration failed: {err}"),
            );
        }
    };
    let ok = summary
        .result
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !ok {
        let _ = snapshot::remove_working_copy(rekeyed_output);
        return step(
            "pcqq_db_rekey_failed",
            false,
            source,
            rekeyed_output,
            format!(
                "KernelUtil rekey did not report ok; process_exit={:?}; timed_out={}; result={}",
                summary.process_exit,
                summary.timed_out,
                summary
                    .result
                    .as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".to_string())
            ),
        );
    }
    match strip_pcqq_header(rekeyed_output, plain_output, options.force)
        .and_then(|_| sqlite_summary(plain_output))
    {
        Ok(detail) => {
            let removed = snapshot::remove_working_copy(rekeyed_output)
                .map(|removed| format!("encrypted_working_copy_removed={removed}"))
                .unwrap_or_else(|err| format!("encrypted_working_copy_remove_failed={err}"));
            step(
                prepared_db_role(source),
                true,
                source,
                plain_output,
                format!(
                    "encrypted_working_copy_method={} bytes={}{}; KernelUtil rekey ok; {detail}; {removed}",
                    working_copy.method,
                    working_copy.bytes,
                    working_copy
                        .fallback_reason
                        .as_ref()
                        .map(|reason| format!(" fallback_reason={reason}"))
                        .unwrap_or_default()
                ),
            )
        }
        Err(err) => {
            let removed = snapshot::remove_working_copy(rekeyed_output)
                .map(|removed| format!("encrypted_working_copy_removed={removed}"))
                .unwrap_or_else(|remove_err| {
                    format!("encrypted_working_copy_remove_failed={remove_err}")
                });
            step(
                "pcqq_db_rekey_strip_failed",
                false,
                source,
                plain_output,
                format!("KernelUtil rekey reported ok but strip/validate failed: {err}; {removed}"),
            )
        }
    }
}

pub fn extract_cfb_step(
    source: &Path,
    prepared_root: &Path,
    account_root: &Path,
    force: bool,
    stream_limit: Option<usize>,
) -> PreprocessStep {
    let rel = source.strip_prefix(account_root).unwrap_or(source);
    let out_base = prepared_root
        .join("pcqq")
        .join("cfb")
        .join(safe_db_name(rel));
    match cfb::extract_cfb_with_limit(source, &out_base, force, stream_limit) {
        Ok(report) => step(
            "pcqq_cfb_extract",
            true,
            source,
            &out_base,
            format!("streams={} bytes={}", report.streams, report.bytes),
        ),
        Err(err) => step(
            "pcqq_cfb_extract",
            false,
            source,
            &out_base,
            err.to_string(),
        ),
    }
}

fn safe_db_name(path: &Path) -> String {
    let text = path.to_string_lossy().replace(['\\', '/'], "__");
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '@' | '+' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "_".to_string()
    } else {
        out
    }
}

fn prepared_db_role(source: &Path) -> &'static str {
    match source.file_name().and_then(|name| name.to_str()) {
        Some("Msg3.0.db") => "msg3_db_prepared",
        Some("Msg3.0index.db") => "msg3_index_db_prepared",
        _ => "pcqq_db_prepared",
    }
}

fn pcqq_plain_output_from_rekeyed(output: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut replaced = false;
    for component in output.components() {
        let text = component.as_os_str().to_string_lossy();
        if !replaced && (text == "rekeyed_with_header" || text == "encrypted-tmp") {
            out.push("db");
            replaced = true;
        } else {
            out.push(component.as_os_str());
        }
    }
    out
}

fn ntqq_clean_output(root: &Path, account: &str, prepared_root: &Path, source: &Path) -> PathBuf {
    if let Ok(rel) = source.strip_prefix(root.join(account)) {
        return prepared_root.join("ntqq").join("clean").join(rel);
    }
    if let Ok(rel) = source.strip_prefix(root) {
        return prepared_root.join("ntqq").join("clean").join(rel);
    }
    prepared_root
        .join("ntqq")
        .join("clean")
        .join(safe_db_name(source))
}

fn discover_classic_dbs(root: &Path, account: &str) -> anyhow::Result<Vec<PathBuf>> {
    let account_root = root.join(account);
    let search_dirs = [
        account_root.clone(),
        account_root.join("QQ"),
        account_root.join("Audio").join("AudioInfo"),
        account_root.join("MyCollection"),
        account_root.join("GroupCollection"),
    ];
    let mut dbs = Vec::new();
    for dir in search_dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("db") {
                continue;
            }
            if is_nt_path(&account_root, &path) {
                continue;
            }
            dbs.push(path);
        }
    }
    dbs.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        mb.cmp(&ma).then_with(|| {
            a.strip_prefix(&account_root)
                .unwrap_or(a)
                .cmp(b.strip_prefix(&account_root).unwrap_or(b))
        })
    });
    Ok(dbs)
}

fn discover_ntqq_dbs(root: &Path, account: &str) -> anyhow::Result<Vec<PathBuf>> {
    let account_root = root.join(account);
    let search_dirs = [
        account_root.join("nt_qq").join("nt_db"),
        account_root.join("nt_qq").join("nt_data"),
        root.join("nt_qq").join("global").join("nt_db"),
    ];
    let mut dbs = Vec::new();
    for dir in search_dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("db") {
                continue;
            }
            dbs.push(path);
        }
    }
    dbs.sort_by(|a, b| {
        a.strip_prefix(root)
            .unwrap_or(a)
            .cmp(b.strip_prefix(root).unwrap_or(b))
    });
    Ok(dbs)
}

fn source_from_selector(root: &Path, account: &str, selector: &str) -> anyhow::Result<PathBuf> {
    let direct = PathBuf::from(selector);
    if direct.is_absolute() && direct.exists() {
        return Ok(direct);
    }
    let account_root = root.join(account);
    let account_relative = account_root.join(selector);
    if account_relative.exists() {
        return Ok(account_relative);
    }
    let matches = discover_classic_dbs(root, account)?
        .into_iter()
        .chain(discover_ntqq_dbs(root, account)?)
        .filter(|path| path.file_name().and_then(|s| s.to_str()) == Some(selector))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => anyhow::bail!("database not found: {selector}"),
        many => anyhow::bail!(
            "selector {selector:?} matched {} files; use account-relative path",
            many.len()
        ),
    }
}

fn is_global_nt_path(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root.join("nt_qq"))
        .ok()
        .is_some_and(|rel| {
            rel.components().any(|part| {
                let text = part.as_os_str().to_string_lossy().to_ascii_lowercase();
                matches!(text.as_str(), "global" | "nt_db" | "nt_data")
            })
        })
}

fn is_nt_path(account_root: &Path, path: &Path) -> bool {
    path.strip_prefix(account_root).ok().is_some_and(|rel| {
        rel.components().any(|part| {
            let text = part.as_os_str().to_string_lossy().to_ascii_lowercase();
            matches!(text.as_str(), "nt_qq" | "nt_db" | "nt_data")
        })
    })
}

fn strip_ntqq_prefix(src: &Path, dst: &Path, force: bool) -> anyhow::Result<()> {
    if dst.exists() && !force {
        let mut file = fs::File::open(dst)?;
        let mut head = [0u8; 16];
        file.read_exact(&mut head)?;
        if &head == b"SQLite format 3\0" {
            return Ok(());
        }
        anyhow::bail!("existing clean copy does not start with a SQLite header");
    }
    let mut input = fs::File::open(src)?;
    let mut head = [0u8; 16];
    input.read_exact(&mut head)?;
    input.seek(SeekFrom::Start(1024))?;
    let mut sqlite_head = [0u8; 16];
    input.read_exact(&mut sqlite_head)?;
    if &head != b"SQLite header 3\0" {
        anyhow::bail!("missing NTQQ extension header");
    }
    if &sqlite_head != b"SQLite format 3\0" {
        anyhow::bail!("inner SQLite header not present at offset 1024");
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_extension(format!(
        "{}.ntqq-stripping",
        dst.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    let mut output = fs::File::create(&tmp)?;
    input.seek(SeekFrom::Start(1024))?;
    std::io::copy(&mut input, &mut output)?;
    output.flush()?;
    fs::rename(tmp, dst)?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum DbHeaderKind {
    PcqqEncryptedSqlite,
    Sqlite,
    OleCompound,
    Unknown([u8; 16]),
}

impl DbHeaderKind {
    fn as_str(&self) -> String {
        match self {
            DbHeaderKind::PcqqEncryptedSqlite => "pcqq-encrypted-sqlite".to_string(),
            DbHeaderKind::Sqlite => "sqlite".to_string(),
            DbHeaderKind::OleCompound => "ole-compound".to_string(),
            DbHeaderKind::Unknown(head) => head.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

fn header_kind(path: &Path) -> anyhow::Result<DbHeaderKind> {
    let mut file = fs::File::open(path)?;
    let mut head = [0u8; 16];
    let read = file.read(&mut head)?;
    if read < 16 {
        return Ok(DbHeaderKind::Unknown(head));
    }
    if &head == b"SQLite header 3\0" {
        return Ok(DbHeaderKind::PcqqEncryptedSqlite);
    }
    if &head == b"SQLite format 3\0" {
        return Ok(DbHeaderKind::Sqlite);
    }
    if head.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
        return Ok(DbHeaderKind::OleCompound);
    }
    Ok(DbHeaderKind::Unknown(head))
}

fn strip_pcqq_header(src: &Path, dst: &Path, force: bool) -> anyhow::Result<()> {
    if dst.exists() && !force {
        sqlite_summary(dst)?;
        return Ok(());
    }
    let mut input = fs::File::open(src)?;
    let mut head = [0u8; 16];
    input.read_exact(&mut head)?;
    input.seek(SeekFrom::Start(1024))?;
    let mut sqlite_head = [0u8; 16];
    input.read_exact(&mut sqlite_head)?;
    if &head != b"SQLite header 3\0" {
        anyhow::bail!("missing PCQQ extension header");
    }
    if &sqlite_head != b"SQLite format 3\0" {
        anyhow::bail!("inner SQLite header not present at offset 1024; rekey required");
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_extension(format!(
        "{}.stripping",
        dst.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    let mut output = fs::File::create(&tmp)?;
    input.seek(SeekFrom::Start(1024))?;
    std::io::copy(&mut input, &mut output)?;
    output.flush()?;
    sqlite_summary(&tmp)?;
    fs::rename(tmp, dst)?;
    Ok(())
}

fn copy_file(src: &Path, dst: &Path, force: bool) -> anyhow::Result<()> {
    if dst.exists() && !force && dst.metadata()?.len() == src.metadata()?.len() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_extension(format!(
        "{}.copying",
        dst.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    fs::copy(src, &tmp)?;
    fs::rename(tmp, dst)?;
    Ok(())
}

fn register_path(name: &str, input: &Path, output: &Path, expect_dir: bool) -> PreprocessStep {
    let ok = if expect_dir {
        input.is_dir()
    } else {
        input.exists()
    };
    let detail = if ok {
        "path available".to_string()
    } else if expect_dir {
        "expected directory is missing".to_string()
    } else {
        "expected path is missing".to_string()
    };
    step(name, ok, input, output, detail)
}

fn sqlite_summary(path: &Path) -> anyhow::Result<String> {
    let con = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let tables: i64 = con.query_row(
        "select count(*) from sqlite_master where type='table'",
        [],
        |row| row.get(0),
    )?;
    if tables <= 0 {
        anyhow::bail!("sqlite readable but schema is empty; refusing to use as prepared DB");
    }
    Ok(format!("sqlite readable; tables={tables}"))
}

fn step(
    name: &str,
    ok: bool,
    input: &Path,
    output: &Path,
    detail: impl Into<String>,
) -> PreprocessStep {
    PreprocessStep {
        name: name.to_string(),
        ok,
        input: input.display().to_string(),
        output: output.display().to_string(),
        detail: detail.into(),
    }
}

pub fn default_preprocess_credential_path(root: &Path, account: &str) -> PathBuf {
    let unified = credentials::default_credentials_path(root, account);
    if unified.is_file() {
        unified
    } else {
        credentials::legacy_infostorage_key_path(root, account)
    }
}

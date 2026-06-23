use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use xz2::read::XzDecoder;

use crate::credentials::{self, CredentialKind, CredentialRecord};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HookKind {
    Infostorage,
    PcqqSqlite,
}

impl HookKind {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "infostorage" | "info-storage" => Ok(Self::Infostorage),
            "pcqq-sqlite" | "pcqq" | "sqlite" => Ok(Self::PcqqSqlite),
            _ => anyhow::bail!("unknown hook kind: {value}"),
        }
    }
}

impl Default for HookKind {
    fn default() -> Self {
        Self::Infostorage
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NormalizeSummary {
    pub input: String,
    pub credentials: String,
    pub events: usize,
    pub imported: usize,
    pub ignored: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CaptureRunOptions {
    pub kind: HookKind,
    pub account: String,
    pub frida: String,
    pub process: Option<String>,
    pub pid: Option<u32>,
    pub spawn: Option<String>,
    pub script_path: std::path::PathBuf,
    pub events_path: std::path::PathBuf,
    pub credentials_path: Option<std::path::PathBuf>,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CaptureRunSummary {
    pub kind: HookKind,
    pub account: String,
    pub frida: String,
    pub target: String,
    pub script: String,
    pub events: String,
    pub process_exit: Option<i32>,
    pub timed_out: bool,
    pub captured_events: usize,
    pub normalize: Option<NormalizeSummary>,
}

#[derive(Clone, Debug, Default)]
pub struct PcqqRekeyOptions {
    pub frida: String,
    pub process: Option<String>,
    pub pid: Option<u32>,
    pub input_path: PathBuf,
    pub key_hex: String,
    pub script_path: PathBuf,
    pub events_path: PathBuf,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PcqqRekeySummary {
    pub frida: String,
    pub target: String,
    pub input: String,
    pub script: String,
    pub events: String,
    pub process_exit: Option<i32>,
    pub timed_out: bool,
    pub captured_events: usize,
    pub result: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub struct PcqqQueryOptions {
    pub frida: String,
    pub process: Option<String>,
    pub pid: Option<u32>,
    pub input_path: PathBuf,
    pub key_hex: String,
    pub sql: String,
    pub max_rows: usize,
    pub script_path: PathBuf,
    pub events_path: PathBuf,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PcqqQuerySummary {
    pub frida: String,
    pub target: String,
    pub input: String,
    pub script: String,
    pub events: String,
    pub process_exit: Option<i32>,
    pub timed_out: bool,
    pub captured_events: usize,
    pub result: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub struct PcqqKeyStorageProbeOptions {
    pub frida: String,
    pub process: Option<String>,
    pub pid: Option<u32>,
    pub account: String,
    pub script_path: PathBuf,
    pub events_path: PathBuf,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PcqqKeyStorageProbeSummary {
    pub account: String,
    pub frida: String,
    pub target: String,
    pub script: String,
    pub events: String,
    pub process_exit: Option<i32>,
    pub timed_out: bool,
    pub captured_events: usize,
}

#[derive(Clone, Debug, Default)]
pub struct FridaDependencyOptions {
    pub cache_root: PathBuf,
    pub version: Option<String>,
    pub force: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FridaDependencySummary {
    pub version: String,
    pub platform: String,
    pub asset_name: String,
    pub url: String,
    pub archive_path: String,
    pub executable_path: String,
    pub downloaded: bool,
    pub extracted: bool,
}

pub fn hook_script(kind: HookKind, account: &str) -> String {
    match kind {
        HookKind::Infostorage => INFOSTORAGE_HOOK_JS.to_string(),
        HookKind::PcqqSqlite => PCQQ_SQLITE_HOOK_JS.replace("__QQ_ANALYZER_ACCOUNT__", account),
    }
}

fn hook_script_for_run(kind: HookKind, account: &str, runner: &str, events_path: &Path) -> String {
    let event_path = process_path_for_runner(runner, events_path);
    let event_path_json = serde_json::to_string(&event_path).unwrap_or_else(|_| "null".to_string());
    let script = hook_script(kind, account);
    let script = script.replace("send(", "qqAnalyzerEmit(");
    format!(
        r#"
const QQ_ANALYZER_EVENT_PATH = {event_path_json};
let qqAnalyzerEventFile = null;
	function qqAnalyzerEmit(obj) {{
	  try {{
	    if (QQ_ANALYZER_EVENT_PATH && qqAnalyzerEventFile === null) {{
	      qqAnalyzerEventFile = new File(QQ_ANALYZER_EVENT_PATH, "a");
	    }}
	    if (qqAnalyzerEventFile !== null) {{
	      qqAnalyzerEventFile.write(JSON.stringify(obj) + "\n");
	      qqAnalyzerEventFile.flush();
	    }}
	  }} catch (e) {{
	  }}
	  send(obj);
	}}
{script}
"#
    )
}

pub fn write_hook_script(kind: HookKind, account: &str, out: &Path) -> anyhow::Result<()> {
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out, hook_script(kind, account))?;
    Ok(())
}

pub fn ensure_frida_inject(
    options: &FridaDependencyOptions,
) -> anyhow::Result<FridaDependencySummary> {
    let version = match options.version.as_deref().map(str::trim) {
        Some("") | None | Some("latest") => latest_frida_version()?,
        Some(version) => version.trim_start_matches('v').to_string(),
    };
    let platform = FridaPlatform::detect()?;
    let asset_name = platform.asset_name(&version);
    let url = format!("https://github.com/frida/frida/releases/download/{version}/{asset_name}");
    let version_dir = options.cache_root.join(&version);
    let archive_path = version_dir.join(&asset_name);
    let executable_path = version_dir.join(platform.executable_name());
    fs::create_dir_all(&version_dir)?;

    let mut downloaded = false;
    if options.force || !archive_path.is_file() {
        download_to_file(&url, &archive_path)?;
        downloaded = true;
    }

    let mut extracted = false;
    if options.force || !executable_path.is_file() {
        decompress_xz_file(&archive_path, &executable_path)?;
        extracted = true;
    }
    make_executable(&executable_path)?;

    Ok(FridaDependencySummary {
        version,
        platform: platform.label().to_string(),
        asset_name,
        url,
        archive_path: archive_path.display().to_string(),
        executable_path: executable_path.display().to_string(),
        downloaded,
        extracted,
    })
}

pub fn run_frida_capture(options: &CaptureRunOptions) -> anyhow::Result<CaptureRunSummary> {
    let target_modes = [
        options.process.is_some(),
        options.pid.is_some(),
        options.spawn.is_some(),
    ]
    .into_iter()
    .filter(|v| *v)
    .count();
    if target_modes != 1 {
        anyhow::bail!("exactly one of process, pid, or spawn must be set");
    }
    if let Some(parent) = options.events_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = options.script_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &options.script_path,
        hook_script_for_run(
            options.kind,
            &options.account,
            &options.frida,
            &options.events_path,
        ),
    )?;
    let target = if let Some(pid) = options.pid {
        pid.to_string()
    } else if let Some(spawn) = &options.spawn {
        spawn.clone()
    } else {
        options.process.clone().unwrap_or_default()
    };
    let mut args = Vec::new();
    if let Some(pid) = options.pid {
        args.push("-p".to_string());
        args.push(pid.to_string());
    } else if let Some(spawn) = &options.spawn {
        args.push("-f".to_string());
        args.push(spawn.clone());
    } else if let Some(process) = &options.process {
        args.push("-n".to_string());
        args.push(process.clone());
    }
    args.push(script_flag_for_runner(&options.frida).to_string());
    args.push(runner_path_arg(&options.frida, &options.script_path));

    let mut child = Command::new(&options.frida)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to start frida {:?}: {err}", options.frida))?;

    let events_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.events_path)?;
    let writer = Arc::new(Mutex::new(EventWriter {
        file: events_file,
        events: 0,
    }));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_event_reader("stdout", stdout, Arc::clone(&writer)));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_event_reader("stderr", stderr, Arc::clone(&writer)));
    }

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status.code().unwrap_or(-1));
        }
        if options
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            timed_out = true;
            child.kill()?;
            break child.wait().ok().map(|status| status.code().unwrap_or(-1));
        }
        thread::sleep(Duration::from_millis(100));
    };
    for reader in readers {
        let _ = reader.join();
    }
    let captured_events = count_event_lines(&options.events_path).unwrap_or_else(|_| {
        writer
            .lock()
            .map(|writer| writer.events)
            .unwrap_or_default()
    });
    let normalize = if let Some(credentials_path) = &options.credentials_path {
        Some(normalize_event_log(
            &options.account,
            &options.events_path,
            credentials_path,
        )?)
    } else {
        None
    };
    Ok(CaptureRunSummary {
        kind: options.kind,
        account: options.account.clone(),
        frida: options.frida.clone(),
        target,
        script: options.script_path.display().to_string(),
        events: options.events_path.display().to_string(),
        process_exit: status,
        timed_out,
        captured_events,
        normalize,
    })
}

pub fn run_pcqq_rekey(options: &PcqqRekeyOptions) -> anyhow::Result<PcqqRekeySummary> {
    if options.process.is_none() == options.pid.is_none() {
        anyhow::bail!("exactly one of process or pid must be set");
    }
    if !options.input_path.is_file() {
        anyhow::bail!(
            "input copied PCQQ DB is missing: {}",
            options.input_path.display()
        );
    }
    if let Some(parent) = options.script_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let key_record = credentials::CredentialRecord {
        kind: CredentialKind::PcqqSqliteKey,
        account: "validate".to_string(),
        source: String::new(),
        db_path: options.input_path.display().to_string(),
        key_hex: options.key_hex.clone(),
        key_len: 0,
        captured_at: String::new(),
        metadata: Default::default(),
    }
    .normalized();
    key_record.validate()?;
    fs::write(
        &options.script_path,
        pcqq_rekey_script_for_run(
            &options.frida,
            &options.events_path,
            &options.input_path,
            &key_record.key_hex,
        ),
    )?;
    if let Some(parent) = options.events_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&options.events_path);

    let mut args = Vec::new();
    let target = if let Some(pid) = options.pid {
        args.push("-p".to_string());
        args.push(pid.to_string());
        pid.to_string()
    } else {
        let process = options.process.clone().unwrap_or_default();
        args.push("-n".to_string());
        args.push(process.clone());
        process
    };
    args.push(script_flag_for_runner(&options.frida).to_string());
    args.push(runner_path_arg(&options.frida, &options.script_path));

    let mut child = Command::new(&options.frida)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to start frida {:?}: {err}", options.frida))?;
    let events_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.events_path)?;
    let writer = Arc::new(Mutex::new(EventWriter {
        file: events_file,
        events: 0,
    }));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_event_reader("stdout", stdout, Arc::clone(&writer)));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_event_reader("stderr", stderr, Arc::clone(&writer)));
    }
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status.code().unwrap_or(-1));
        }
        if options
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            timed_out = true;
            child.kill()?;
            break child.wait().ok().map(|status| status.code().unwrap_or(-1));
        }
        if rekey_result_from_events(&options.events_path)?.is_some() {
            child.kill()?;
            break child.wait().ok().map(|status| status.code().unwrap_or(-1));
        }
        thread::sleep(Duration::from_millis(100));
    };
    for reader in readers {
        let _ = reader.join();
    }
    let captured_events = count_event_lines(&options.events_path).unwrap_or_else(|_| {
        writer
            .lock()
            .map(|writer| writer.events)
            .unwrap_or_default()
    });
    Ok(PcqqRekeySummary {
        frida: options.frida.clone(),
        target,
        input: options.input_path.display().to_string(),
        script: options.script_path.display().to_string(),
        events: options.events_path.display().to_string(),
        process_exit: status,
        timed_out,
        captured_events,
        result: rekey_result_from_events(&options.events_path)?,
    })
}

pub fn run_pcqq_query(options: &PcqqQueryOptions) -> anyhow::Result<PcqqQuerySummary> {
    if options.process.is_none() == options.pid.is_none() {
        anyhow::bail!("exactly one of process or pid must be set");
    }
    if !options.input_path.is_file() {
        anyhow::bail!(
            "input copied PCQQ DB is missing: {}",
            options.input_path.display()
        );
    }
    let key_record = credentials::CredentialRecord {
        kind: CredentialKind::PcqqSqliteKey,
        account: "validate".to_string(),
        source: String::new(),
        db_path: options.input_path.display().to_string(),
        key_hex: options.key_hex.clone(),
        key_len: 0,
        captured_at: String::new(),
        metadata: Default::default(),
    }
    .normalized();
    key_record.validate()?;
    if let Some(parent) = options.script_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = options.events_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &options.script_path,
        pcqq_query_script_for_run(
            &options.frida,
            &options.events_path,
            &options.input_path,
            &key_record.key_hex,
            &options.sql,
            options.max_rows,
        ),
    )?;
    let _ = fs::remove_file(&options.events_path);

    let mut args = Vec::new();
    let target = if let Some(pid) = options.pid {
        args.push("-p".to_string());
        args.push(pid.to_string());
        pid.to_string()
    } else {
        let process = options.process.clone().unwrap_or_default();
        args.push("-n".to_string());
        args.push(process.clone());
        process
    };
    args.push(script_flag_for_runner(&options.frida).to_string());
    args.push(runner_path_arg(&options.frida, &options.script_path));

    let mut child = Command::new(&options.frida)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to start frida {:?}: {err}", options.frida))?;
    let events_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.events_path)?;
    let writer = Arc::new(Mutex::new(EventWriter {
        file: events_file,
        events: 0,
    }));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_event_reader("stdout", stdout, Arc::clone(&writer)));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_event_reader("stderr", stderr, Arc::clone(&writer)));
    }
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status.code().unwrap_or(-1));
        }
        if options
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            timed_out = true;
            child.kill()?;
            break child.wait().ok().map(|status| status.code().unwrap_or(-1));
        }
        if pcqq_query_result_from_events(&options.events_path)?.is_some() {
            child.kill()?;
            break child.wait().ok().map(|status| status.code().unwrap_or(-1));
        }
        thread::sleep(Duration::from_millis(100));
    };
    for reader in readers {
        let _ = reader.join();
    }
    let captured_events = count_event_lines(&options.events_path).unwrap_or_else(|_| {
        writer
            .lock()
            .map(|writer| writer.events)
            .unwrap_or_default()
    });
    Ok(PcqqQuerySummary {
        frida: options.frida.clone(),
        target,
        input: options.input_path.display().to_string(),
        script: options.script_path.display().to_string(),
        events: options.events_path.display().to_string(),
        process_exit: status,
        timed_out,
        captured_events,
        result: pcqq_query_result_from_events(&options.events_path)?,
    })
}

pub fn run_pcqq_key_storage_probe(
    options: &PcqqKeyStorageProbeOptions,
) -> anyhow::Result<PcqqKeyStorageProbeSummary> {
    if options.process.is_none() == options.pid.is_none() {
        anyhow::bail!("exactly one of process or pid must be set");
    }
    if let Some(parent) = options.script_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = options.events_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&options.events_path);
    fs::write(
        &options.script_path,
        pcqq_key_storage_probe_script_for_run(
            &options.frida,
            &options.events_path,
            &options.account,
        ),
    )?;

    let mut args = Vec::new();
    let target = if let Some(pid) = options.pid {
        args.push("-p".to_string());
        args.push(pid.to_string());
        pid.to_string()
    } else {
        let process = options.process.clone().unwrap_or_default();
        args.push("-n".to_string());
        args.push(process.clone());
        process
    };
    args.push(script_flag_for_runner(&options.frida).to_string());
    args.push(runner_path_arg(&options.frida, &options.script_path));

    let mut child = Command::new(&options.frida)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to start frida {:?}: {err}", options.frida))?;
    let events_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.events_path)?;
    let writer = Arc::new(Mutex::new(EventWriter {
        file: events_file,
        events: 0,
    }));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_event_reader("stdout", stdout, Arc::clone(&writer)));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_event_reader("stderr", stderr, Arc::clone(&writer)));
    }
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status.code().unwrap_or(-1));
        }
        if options
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            timed_out = true;
            child.kill()?;
            break child.wait().ok().map(|status| status.code().unwrap_or(-1));
        }
        thread::sleep(Duration::from_millis(100));
    };
    for reader in readers {
        let _ = reader.join();
    }
    let captured_events = count_event_lines(&options.events_path).unwrap_or_else(|_| {
        writer
            .lock()
            .map(|writer| writer.events)
            .unwrap_or_default()
    });
    Ok(PcqqKeyStorageProbeSummary {
        account: options.account.clone(),
        frida: options.frida.clone(),
        target,
        script: options.script_path.display().to_string(),
        events: options.events_path.display().to_string(),
        process_exit: status,
        timed_out,
        captured_events,
    })
}

fn script_flag_for_runner(runner: &str) -> &'static str {
    let name = Path::new(runner)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(runner)
        .to_ascii_lowercase();
    if name.contains("frida-inject") {
        "-s"
    } else {
        "-l"
    }
}

fn runner_path_arg(runner: &str, path: &Path) -> String {
    process_path_for_runner(runner, path)
}

fn process_path_for_runner(runner: &str, path: &Path) -> String {
    let owned_path;
    let path = if path.is_absolute() {
        path
    } else {
        owned_path = std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf());
        &owned_path
    };
    let name = Path::new(runner)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(runner)
        .to_ascii_lowercase();
    if name.ends_with(".exe") && HostPlatform::detect() == HostPlatform::Wsl {
        wsl_path_to_windows(path)
    } else {
        path.display().to_string()
    }
}

fn wsl_path_to_windows(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix("/mnt/") {
        let mut parts = rest.splitn(2, '/');
        if let (Some(drive), Some(tail)) = (parts.next(), parts.next()) {
            if drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic() {
                return format!(
                    "{}:\\{}",
                    drive.to_ascii_uppercase(),
                    tail.replace('/', "\\")
                );
            }
        }
    }
    text
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostPlatform {
    Windows,
    Wsl,
    Linux,
    Macos,
    Other,
}

impl HostPlatform {
    fn detect() -> Self {
        if cfg!(windows) {
            return Self::Windows;
        }
        if cfg!(target_os = "macos") {
            return Self::Macos;
        }
        if cfg!(target_os = "linux") {
            if std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .map(|text| {
                    text.to_ascii_lowercase().contains("microsoft")
                        || text.to_ascii_lowercase().contains("wsl")
                })
                .unwrap_or(false)
            {
                return Self::Wsl;
            }
            return Self::Linux;
        }
        Self::Other
    }
}

#[derive(Clone, Copy, Debug)]
enum FridaPlatform {
    WindowsX86_64,
    LinuxX86_64,
    MacosUniversal,
}

impl FridaPlatform {
    fn detect() -> anyhow::Result<Self> {
        let arch = std::env::consts::ARCH;
        match HostPlatform::detect() {
            HostPlatform::Windows | HostPlatform::Wsl if arch == "x86_64" => Ok(Self::WindowsX86_64),
            HostPlatform::Linux if arch == "x86_64" => Ok(Self::LinuxX86_64),
            HostPlatform::Macos => Ok(Self::MacosUniversal),
            other => anyhow::bail!(
                "automatic frida-inject download is not supported on host={other:?} arch={arch}; pass --frida <path>"
            ),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "windows-x86_64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::MacosUniversal => "macos-universal",
        }
    }

    fn executable_name(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "frida-inject.exe",
            Self::LinuxX86_64 | Self::MacosUniversal => "frida-inject",
        }
    }

    fn asset_name(self, version: &str) -> String {
        match self {
            Self::WindowsX86_64 => {
                format!("frida-inject-{version}-windows-x86_64.exe.xz")
            }
            Self::LinuxX86_64 => format!("frida-inject-{version}-linux-x86_64.xz"),
            Self::MacosUniversal => format!("frida-inject-{version}-macos-universal.xz"),
        }
    }
}

fn pcqq_rekey_script(path: &str, key_hex: &str) -> String {
    let path_json = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".to_string());
    let key_json = serde_json::to_string(key_hex).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r###"
const QQ_ANALYZER_REKEY_PATH = {path_json};
const QQ_ANALYZER_REKEY_KEY_HEX = {key_json};
const kernel_util = Module.load('KernelUtil.dll');
function single_function(pattern) {{
  pattern = pattern.replaceAll("##", "").replaceAll(" ", "").toLowerCase().replace(/\s/g,'').replace(/(.{{2}})/g,"$1 ");
  const hits = Memory.scanSync(kernel_util.base, kernel_util.size, pattern);
  if (hits.length !== 1) {{
    send({{type: "rekey_result", ok: false, stage: "signature", pattern: pattern, hits: hits.length}});
    throw new Error("KernelUtil signature mismatch");
  }}
  return hits[0].address;
}}
function hexBytes(hex) {{
  const out = [];
  for (let i = 0; i < hex.length; i += 2) out.push(parseInt(hex.substr(i, 2), 16));
  return out;
}}
const open_function = single_function("##558BEC6A006A06FF750CFF7508E8E0130200");
	const key_function = single_function("55 8b ec 56 6b 75 10 11 83 7d 10 10 74 0d 68 17 02 00 00 e8");
	const rekey_function = single_function("##558BEC837D1010740D682F020000E8");
	const close_function = single_function("##55 8B EC 56 8B 75 08 85 F6 74 6D 56 E87C 3E 01 00");
	const open_db = new NativeFunction(open_function, 'int', ['pointer', 'pointer']);
	const key_db = new NativeFunction(key_function, 'int', ['pointer', 'pointer', 'int']);
	const rekey_db = new NativeFunction(rekey_function, 'int', ['pointer', 'pointer', 'int']);
	const close_db = new NativeFunction(close_function, 'int', ['pointer', 'int']);
	function rekeyCopy(filePath, keyHex) {{
	  const keyBytes = hexBytes(keyHex);
	  const handlePtr = Memory.alloc(Process.pointerSize);
	  const openRet = open_db(Memory.allocUtf8String(filePath), handlePtr);
  const db = handlePtr.readPointer();
  if (openRet !== 0 || db.isNull()) return {{type: "rekey_result", ok: false, stage: "open", path: filePath, openRet: openRet, db: db.toString()}};
  const keyPtr = Memory.alloc(keyBytes.length);
  keyPtr.writeByteArray(keyBytes);
	  const emptyPtr = Memory.alloc(keyBytes.length);
	  emptyPtr.writeByteArray(Array(keyBytes.length).fill(0));
	  const keyRet = key_db(db, keyPtr, keyBytes.length);
	  let rekeyRet = -999;
	  if (keyRet === 0) {{
	    rekeyRet = rekey_db(db, emptyPtr, keyBytes.length);
	  }}
	  const closeRet = close_db(db, 0);
	  return {{
	    type: "rekey_result",
	    ok: rekeyRet === 0,
	    stage: "rekey",
	    path: filePath,
	    key_len: keyBytes.length,
	    openRet: openRet,
	    keyRet: keyRet,
	    rekeyRet: rekeyRet,
	    closeRet: closeRet
	  }};
	}}
send(rekeyCopy(QQ_ANALYZER_REKEY_PATH, QQ_ANALYZER_REKEY_KEY_HEX));
"###
    )
}

fn pcqq_rekey_script_for_run(
    runner: &str,
    events_path: &Path,
    input_path: &Path,
    key_hex: &str,
) -> String {
    let event_path = process_path_for_runner(runner, events_path);
    let event_path_json = serde_json::to_string(&event_path).unwrap_or_else(|_| "null".to_string());
    let input_path = process_path_for_runner(runner, input_path);
    let script = pcqq_rekey_script(&input_path, key_hex);
    let script = script.replace("send(", "qqAnalyzerEmit(");
    format!(
        r#"
	const QQ_ANALYZER_EVENT_PATH = {event_path_json};
	let qqAnalyzerEventFile = null;
	function qqAnalyzerEmit(obj) {{
	  let wrote = false;
	  try {{
	    if (QQ_ANALYZER_EVENT_PATH && qqAnalyzerEventFile === null) {{
	      qqAnalyzerEventFile = new File(QQ_ANALYZER_EVENT_PATH, "a");
	    }}
	    if (qqAnalyzerEventFile !== null) {{
	      qqAnalyzerEventFile.write(JSON.stringify(obj) + "\n");
	      qqAnalyzerEventFile.flush();
	      wrote = true;
	    }}
	  }} catch (e) {{
	  }}
	  if (wrote) {{
	    send({{type: "_event_written", payload_type: obj && obj.type ? obj.type : null}});
	  }} else {{
	    send(obj);
	  }}
	}}
{script}
"#
    )
}

fn pcqq_query_script(path: &str, key_hex: &str, sql: &str, max_rows: usize) -> String {
    let path_json = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".to_string());
    let key_json = serde_json::to_string(key_hex).unwrap_or_else(|_| "\"\"".to_string());
    let sql_json = serde_json::to_string(sql).unwrap_or_else(|_| "\"\"".to_string());
    let max_rows = max_rows.max(1);
    format!(
        r###"
const QQ_ANALYZER_QUERY_PATH = {path_json};
const QQ_ANALYZER_QUERY_KEY_HEX = {key_json};
const QQ_ANALYZER_QUERY_SQL = {sql_json};
const QQ_ANALYZER_QUERY_MAX_ROWS = {max_rows};

function hexBytes(hex) {{
  const out = [];
  for (let i = 0; i < hex.length; i += 2) out.push(parseInt(hex.substr(i, 2), 16));
  return out;
}}
function ptrText(p) {{
  try {{ return p.isNull() ? null : p.readUtf8String(); }} catch (e) {{ return "<readUtf8String failed: " + e + ">"; }}
}}
const kernelUtil = Process.getModuleByName("KernelUtil.dll");
function rva(x) {{ return kernelUtil.base.add(x); }}
const ctorDb = new NativeFunction(rva(0x12f530), "pointer", ["pointer"], "thiscall");
const openDb = new NativeFunction(rva(0x130faa), "int", ["pointer", "pointer"], "thiscall");
const keyDb = new NativeFunction(rva(0x2f9c), "void", ["pointer", "pointer", "int"], "thiscall");
const closeDb = new NativeFunction(rva(0x1300db), "void", ["pointer"], "thiscall");
const execQuery = new NativeFunction(rva(0x1304b6), "pointer", ["pointer", "pointer", "pointer", "pointer"], "thiscall");
const eof = new NativeFunction(rva(0x130223), "bool", ["pointer"], "thiscall");
const nextRow = new NativeFunction(rva(0x130f1f), "void", ["pointer"], "thiscall");
const numFields = new NativeFunction(rva(0x130f83), "int", ["pointer"], "thiscall");
const fieldName = new NativeFunction(rva(0x130801), "pointer", ["pointer", "int"], "thiscall");
const fieldValue = new NativeFunction(rva(0x1308a2), "pointer", ["pointer", "int"], "thiscall");
const finalizeQuery = new NativeFunction(rva(0x130a35), "void", ["pointer"], "thiscall");

function runQuery() {{
  const db = Memory.alloc(0x40);
  ctorDb(db);
  const openRet = openDb(db, Memory.allocUtf8String(QQ_ANALYZER_QUERY_PATH));
  let keyOk = false;
  let sqliteCode = -1;
  let fields = -1;
  let columns = [];
  let rows = [];
  let error = null;
  let truncated = false;
  try {{
    if (openRet === 0) {{
      const keyBytes = hexBytes(QQ_ANALYZER_QUERY_KEY_HEX);
      const keyPtr = Memory.alloc(keyBytes.length);
      keyPtr.writeByteArray(keyBytes);
      keyDb(db, keyPtr, keyBytes.length);
      keyOk = true;
      const query = Memory.alloc(0x40);
      const codePtr = Memory.alloc(4);
      codePtr.writeS32(-1);
      execQuery(db, query, Memory.allocUtf8String(QQ_ANALYZER_QUERY_SQL), codePtr);
      sqliteCode = codePtr.readS32();
      fields = numFields(query);
      for (let i = 0; i < fields; i++) columns.push(ptrText(fieldName(query, i)));
      let guard = 0;
      while (!eof(query)) {{
        if (guard >= QQ_ANALYZER_QUERY_MAX_ROWS) {{
          truncated = true;
          break;
        }}
        const row = [];
        for (let i = 0; i < fields; i++) row.push(ptrText(fieldValue(query, i)));
        rows.push(row);
        nextRow(query);
        guard++;
      }}
      finalizeQuery(query);
    }}
  }} catch (e) {{
    error = String(e);
  }}
  try {{ closeDb(db); }} catch (e) {{ if (error === null) error = "close failed: " + e; }}
  return {{
    type: "pcqq_query_result",
    ok: openRet === 0 && keyOk && error === null,
    path: QQ_ANALYZER_QUERY_PATH,
    sql: QQ_ANALYZER_QUERY_SQL,
    max_rows: QQ_ANALYZER_QUERY_MAX_ROWS,
    openRet: openRet,
    keyOk: keyOk,
    sqlite_code: sqliteCode,
    fields: fields,
    columns: columns,
    rows: rows,
    row_count: rows.length,
    truncated: truncated,
    error: error
  }};
}}
const result = runQuery();
result.ok = result.openRet === 0 && result.keyOk && result.error === null && (result.sqlite_code === 100 || result.sqlite_code === 101);
send(result);
"###
    )
}

fn pcqq_query_script_for_run(
    runner: &str,
    events_path: &Path,
    input_path: &Path,
    key_hex: &str,
    sql: &str,
    max_rows: usize,
) -> String {
    let event_path = process_path_for_runner(runner, events_path);
    let event_path_json = serde_json::to_string(&event_path).unwrap_or_else(|_| "null".to_string());
    let input_path = process_path_for_runner(runner, input_path);
    let script = pcqq_query_script(&input_path, key_hex, sql, max_rows);
    let script = script.replace("send(", "qqAnalyzerEmit(");
    format!(
        r#"
        const QQ_ANALYZER_EVENT_PATH = {event_path_json};
        let qqAnalyzerEventFile = null;
        function qqAnalyzerEmit(obj) {{
          let wrote = false;
          try {{
            if (QQ_ANALYZER_EVENT_PATH && qqAnalyzerEventFile === null) {{
              qqAnalyzerEventFile = new File(QQ_ANALYZER_EVENT_PATH, "a");
            }}
            if (qqAnalyzerEventFile !== null) {{
              qqAnalyzerEventFile.write(JSON.stringify(obj) + "\n");
              qqAnalyzerEventFile.flush();
              wrote = true;
            }}
          }} catch (e) {{
          }}
          if (wrote) {{
            send({{type: "_event_written", payload_type: obj && obj.type ? obj.type : null}});
          }} else {{
            send(obj);
          }}
        }}
{script}
"#
    )
}

fn pcqq_key_storage_probe_script(account: &str) -> String {
    let account_json = serde_json::to_string(account).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r###"
	const QQ_ANALYZER_ACCOUNT = {account_json};
	const kernel_util = Module.load('KernelUtil.dll');
	function kernelStatic(addr) {{
	  return kernel_util.base.add(ptr(addr).sub(ptr('0x56d50000')));
	}}
	function basename(p) {{
  return String(p).replaceAll('/', '\\').split('\\').pop().toLowerCase();
}}
function shouldHandle(p, keyLen) {{
  const n = String(p).replaceAll('/', '\\').toLowerCase();
  if (keyLen !== 16) return false;
  if (!n.endsWith(".db")) return false;
  if (QQ_ANALYZER_ACCOUNT && n.indexOf("\\" + QQ_ANALYZER_ACCOUNT.toLowerCase() + "\\") < 0) return false;
  if (n.indexOf("\\nt_qq\\") >= 0 || n.indexOf("\\nt_db\\") >= 0 || n.indexOf("\\nt_data\\") >= 0) return false;
  return true;
}}
function readPointerSafe(p) {{
  try {{
    if (p.isNull()) return NULL;
    return p.readPointer();
  }} catch (e) {{
    return NULL;
  }}
}}
function ptrString(p) {{
  try {{ return p.toString(); }} catch (e) {{ return "<ptr failed: " + e + ">"; }}
}}
function readU32Safe(p) {{
  try {{
    if (p.isNull()) return null;
    return p.readU32() >>> 0;
  }} catch (e) {{
    return null;
  }}
}}
	function readableRange(ptr, len) {{
	  const range = Process.findRangeByAddress(ptr);
	  if (!range || range.protection.indexOf('r') < 0) return false;
	  return ptr.add(len).compare(range.base.add(range.size)) <= 0;
	}}
	function plausibleHeapPointer(ptr, len) {{
	  if (ptr.isNull()) return false;
	  const range = Process.findRangeByAddress(ptr);
	  if (!range || range.protection.indexOf('r') < 0) return false;
	  if (range.protection.indexOf('w') < 0) return false;
	  return ptr.add(len).compare(range.base.add(range.size)) <= 0;
	}}
	function decodeObfuscatedSqliteKey(blockPtr, blockLen) {{
	  if (blockLen !== 0x110 || !plausibleHeapPointer(blockPtr, blockLen)) return null;
	  const bytes = [];
	  for (let i = 0; i < 16; i++) {{
	    const base = i * 0x11;
	    const selector = blockPtr.add(base + 0x10).readU8() & 0x0f;
	    bytes.push(blockPtr.add(base + selector).readU8());
	  }}
	  return bytes.map(b => ("00" + b.toString(16)).slice(-2)).join("");
	}}
	function readCodecObject(codecPtr) {{
	  if (!plausibleHeapPointer(codecPtr, 0x18)) return null;
	  const keyPtr = readPointerSafe(codecPtr.add(0));
	  const keyLen = readU32Safe(codecPtr.add(4));
	  const rekeyPtr = readPointerSafe(codecPtr.add(8));
	  const rekeyLen = readU32Safe(codecPtr.add(12));
	  const pageSize = readU32Safe(codecPtr.add(16));
	  const pageBuffer = readPointerSafe(codecPtr.add(20));
	  const keyHex = decodeObfuscatedSqliteKey(keyPtr, keyLen);
	  const rekeyHex = decodeObfuscatedSqliteKey(rekeyPtr, rekeyLen);
	  return {{
	    codec_state: ptrString(codecPtr),
	    key_ptr: ptrString(keyPtr),
	    key_len: keyLen,
	    key_hex: keyHex,
	    rekey_ptr: ptrString(rekeyPtr),
	    rekey_len: rekeyLen,
	    rekey_hex: rekeyHex,
	    page_size: pageSize,
	    page_buffer: ptrString(pageBuffer)
	  }};
	}}
	function readSqlite3CodecState(sqliteDb) {{
	  const codecState = sqliteDb.isNull() ? NULL : readPointerSafe(sqliteDb.add(0xd4));
	  const codec = codecState.isNull() ? null : readCodecObject(codecState);
	  return {{
	    sqlite_db: ptrString(sqliteDb),
	    codec_state: ptrString(codecState),
	    codec: codec
	  }};
	}}
	function readSqliteConnectionCodecState(conn, slot) {{
	  const table = readPointerSafe(conn.add(0x14));
	  const entry = table.isNull() ? NULL : readPointerSafe(table.add(4 + slot * 0x10));
	  const sqliteDbPtrPtr = entry.isNull() ? NULL : readPointerSafe(entry.add(4));
	  const sqliteDb = sqliteDbPtrPtr.isNull() ? NULL : readPointerSafe(sqliteDbPtrPtr);
	  const state = readSqlite3CodecState(sqliteDb);
	  state.connection = ptrString(conn);
	  state.table = ptrString(table);
	  state.entry = ptrString(entry);
	  state.sqlite_db_ptr_ptr = ptrString(sqliteDbPtrPtr);
	  return state;
	}}
	function managerAddressFromCandidate(candidate) {{
	  if (candidate.isNull() || !readableRange(candidate, 0x74)) return NULL;
	  const vptr0 = readPointerSafe(candidate.add(0));
	  const vptr1 = readPointerSafe(candidate.add(4));
	  if (vptr0.isNull() || vptr1.isNull()) return NULL;
	  if (typeof MSGINDEX_VPTR0 !== 'undefined' && !vptr0.equals(MSGINDEX_VPTR0)) return NULL;
	  if (typeof MSGINDEX_VPTR1 !== 'undefined' && !vptr1.equals(MSGINDEX_VPTR1)) return NULL;
	  return candidate;
	}}
	function sqliteDbName(db) {{
	  try {{ return name_db(db, NULL).readUtf8String(); }} catch (e) {{ return null; }}
	}}
	function dumpManagerHandles(manager, reason) {{
	  manager = managerAddressFromCandidate(manager);
	  if (manager.isNull()) {{
	    send({{type: "key_storage_msgindex_manager", reason: reason, ok: false, error: "invalid manager", manager: ptrString(manager)}});
	    return;
	  }}
	  const start = readPointerSafe(manager.add(0x5c));
	  const end = readPointerSafe(manager.add(0x60));
	  const startNum = start.toUInt32();
	  const endNum = end.toUInt32();
	  let count = 0;
	  if (!start.isNull() && !end.isNull() && endNum >= startNum && endNum - startNum <= 0x10000) {{
	    count = Math.floor((endNum - startNum) / 8);
	  }}
	  send({{
	    type: "key_storage_msgindex_manager",
	    reason: reason,
	    ok: true,
	    manager: ptrString(manager),
	    vector_start: ptrString(start),
	    vector_end: ptrString(end),
	    handle_count: count,
	    ghidra_static: {{
	      image_base: "55ef0000",
	      ctor: "55f5152b",
	      init_msg_index: "55f503ac",
	      prepare_search_msg: "55f50423",
	      get_sqlite_handle_finished: "55f51ebf",
	      reset_sqlite_handle: "55f50937",
	      handle_vector_start_offset: "0x5c",
	      handle_vector_end_offset: "0x60"
	    }}
	  }});
	  for (let i = 0; i < count && i < 256; i++) {{
	    const entry = start.add(i * 8);
	    const db = readPointerSafe(entry);
	    const used = readU32Safe(entry.add(4));
	    if (db.isNull()) continue;
	    const name = sqliteDbName(db);
		    const codec = readSqliteConnectionCodecState(db, 0);
	    send({{
	      type: "key_storage_msgindex_handle",
	      reason: reason,
	      manager: ptrString(manager),
	      index: i,
	      entry: ptrString(entry),
	      db: ptrString(db),
	      used: used,
	      path: name,
	      basename: name ? basename(name) : null,
	      codec: codec
	    }});
	  }}
	}}
	const key_function = kernelStatic('0x56dd6eb2');
	const name_function = kernelStatic('0x56d88548');
	const name_db = new NativeFunction(name_function, 'pointer', ['pointer', 'pointer']);
	const im = Process.findModuleByName('IM.dll');
	function imStatic(addr) {{
	  if (!im) return NULL;
	  return im.base.add(ptr(addr).sub(ptr('0x55ef0000')));
	}}
	const MSGINDEX_VPTR0 = imStatic('0x562d72f4');
	const MSGINDEX_VPTR1 = imStatic('0x562d731c');
	function u32Pattern(value) {{
	  const bytes = [];
	  for (let i = 0; i < 4; i++) bytes.push(("00" + ((value >>> (8 * i)) & 0xff).toString(16)).slice(-2));
	  return bytes.join(" ");
	}}
	function pointerPairPattern(a, b) {{
	  return u32Pattern(a.toUInt32()) + " " + u32Pattern(b.toUInt32());
	}}
	function discoverMsgIndexManagers() {{
	  if (!im || MSGINDEX_VPTR0.isNull() || MSGINDEX_VPTR1.isNull()) {{
	    send({{type: "key_storage_msgindex_discovery", ok: false, reason: "IM.dll/vptr unavailable"}});
	    return;
	  }}
	  const pattern = pointerPairPattern(MSGINDEX_VPTR0, MSGINDEX_VPTR1);
	  const seen = {{}};
	  let ranges = 0;
	  let candidates = 0;
	  let managers = 0;
	  for (const range of Process.enumerateRanges({{ protection: 'rw-', coalesce: true }})) {{
	    ranges += 1;
	    if (range.size < 0x74) continue;
	    let hits = [];
	    try {{ hits = Memory.scanSync(range.base, range.size, pattern); }} catch (e) {{ continue; }}
	    for (const hit of hits) {{
	      candidates += 1;
	      const manager = managerAddressFromCandidate(hit.address);
	      const key = ptrString(manager);
	      if (!manager.isNull() && !seen[key]) {{
	        seen[key] = true;
	        managers += 1;
	        dumpManagerHandles(manager, "vptr_discovery");
	      }}
	    }}
	  }}
	  send({{
	    type: "key_storage_msgindex_discovery",
	    ok: true,
	    pattern: pattern,
	    ranges: ranges,
	    candidates: candidates,
	    managers: managers,
	    vptr0: ptrString(MSGINDEX_VPTR0),
	    vptr1: ptrString(MSGINDEX_VPTR1)
	  }});
	}}
	const MULTI_SQLITE_VPTR = kernelStatic('0x56ea882c');
	function discoverMultiSqliteDbs() {{
	  if (MULTI_SQLITE_VPTR.isNull()) {{
	    send({{type: "key_storage_multisqlite_discovery", ok: false, reason: "vptr unavailable"}});
	    return;
	  }}
	  const pattern = u32Pattern(MULTI_SQLITE_VPTR.toUInt32());
	  const seen = {{}};
	  let ranges = 0;
	  let candidates = 0;
	  let dbs = 0;
	  for (const range of Process.enumerateRanges({{ protection: 'rw-', coalesce: true }})) {{
	    ranges += 1;
	    if (range.size < 0x44) continue;
	    let hits = [];
	    try {{ hits = Memory.scanSync(range.base, range.size, pattern); }} catch (e) {{ continue; }}
	    for (const hit of hits) {{
	      candidates += 1;
	      const db = hit.address;
	      if (!readableRange(db, 0x44)) continue;
	      if (!readPointerSafe(db).equals(MULTI_SQLITE_VPTR)) continue;
	      if (!readPointerSafe(db.add(0x40)).equals(db)) continue;
	      const key = ptrString(db);
	      if (seen[key]) continue;
	      seen[key] = true;
	      dbs += 1;
	      const innerDb = db.add(0x8);
	      const conn = readPointerSafe(innerDb.add(0x4));
		      let name = null;
		      try {{ name = conn.isNull() ? null : name_db(conn, NULL).readUtf8String(); }} catch (e) {{ name = null; }}
		      const codec = conn.isNull() ? readSqlite3CodecState(NULL) : readSqliteConnectionCodecState(conn, 0);
		      const keyHex = codec.codec ? (codec.codec.key_hex || codec.codec.rekey_hex) : null;
		      if (keyHex) {{
		        send({{
		          type: "pcqq_sqlite_memory_key",
		          path: name,
		          basename: name ? basename(name) : null,
		          key_hex: keyHex,
		          key_len: keyHex.length / 2,
		          db: ptrString(db),
		          inner_db: ptrString(innerDb),
		          connection: ptrString(conn),
		          sqlite_db: codec.sqlite_db || null,
		          codec_state: codec.codec_state || null
		        }});
		      }}
		      const codecDetails = codec.codec || null;
		      send({{
		        type: "key_storage_multisqlite_db",
		        db: ptrString(db),
	        inner_db: ptrString(innerDb),
		        connection: ptrString(conn),
		        path: name,
		        basename: name ? basename(name) : null,
		        codec: {{
		          sqlite_db: codec.sqlite_db || null,
		          codec_state: codec.codec_state || null,
		          connection: codec.connection || null,
		          table: codec.table || null,
		          entry: codec.entry || null,
		          sqlite_db_ptr_ptr: codec.sqlite_db_ptr_ptr || null,
		          has_key: keyHex ? true : false,
		          key_len: codecDetails ? codecDetails.key_len : null,
		          page_size: codecDetails ? codecDetails.page_size : null
		        }},
		        ghidra_static: {{
	          image_base: "56d50000",
	          ctor: "56d82842",
	          dtor: "56d82998",
	          vptr: "56ea882c",
	          cppsqlite3db_offset: "0x8",
	          self_pointer_offset: "0x40"
	        }}
	      }});
	    }}
	  }}
	  send({{
	    type: "key_storage_multisqlite_discovery",
	    ok: true,
	    pattern: pattern,
	    ranges: ranges,
	    candidates: candidates,
	    dbs: dbs,
	    vptr: ptrString(MULTI_SQLITE_VPTR)
	  }});
	}}
	send({{
	  type: "hooked",
	  module: "KernelUtil.dll",
	  hook: "pcqq_key_storage_probe",
  key_function: key_function.toString(),
  name_function: name_function.toString(),
  account: QQ_ANALYZER_ACCOUNT,
  ghidra_static: {{
    image_base: "56d50000",
    key_function: "56dd6eb2",
    name_function: "56d88548",
    sqlite_install_codec: "56da7632",
	    codec_state_offset: "0xd4"
	  }},
	  im: im ? {{base: im.base.toString(), size: im.size}} : null
	}});
	if (im) {{
	  const managerHooks = [
	    ["ctor", "55f5152b", "ecx"],
	    ["init_msg_index", "55f503ac", "arg0"],
	    ["prepare_search_msg", "55f50423", "arg0"],
	    ["get_sqlite_handle_finished", "55f51ebf", "ecx"],
	    ["reset_sqlite_handle", "55f50937", "ecx"]
	  ];
	  for (const hook of managerHooks) {{
	    const label = hook[0];
	    const addr = imStatic("0x" + hook[1]);
	    const source = hook[2];
	    try {{
	      Interceptor.attach(addr, {{
	        onEnter: function(args) {{
	          const manager = source === "arg0" ? args[0] : this.context.ecx;
	          dumpManagerHandles(manager, label + ":enter");
	        }},
	        onLeave: function(retval) {{
	          if (label === "ctor") dumpManagerHandles(retval, label + ":leave");
	        }}
	      }});
	    }} catch (e) {{
	      send({{type: "key_storage_msgindex_hook_error", label: label, address: addr.toString(), error: String(e)}});
	    }}
	  }}
	}} else {{
	  send({{type: "key_storage_msgindex_unavailable", reason: "IM.dll not loaded"}});
	}}
	setTimeout(discoverMsgIndexManagers, 0);
	setTimeout(discoverMultiSqliteDbs, 0);
	Interceptor.attach(key_function, {{
	  onEnter: function(args) {{
	    this.db = args[0];
    this.key_len_arg = args[2].toInt32();
    this.db_name = null;
	    try {{ this.db_name = name_db(args[0], NULL).readUtf8String(); }} catch (e) {{ return; }}
	    if (!shouldHandle(this.db_name, this.key_len_arg)) return;
	    this.before = readSqliteConnectionCodecState(this.db, 0);
	  }},
	  onLeave: function(retval) {{
	    if (!this.db_name || !shouldHandle(this.db_name, this.key_len_arg)) return;
	    const after = readSqliteConnectionCodecState(this.db, 0);
    send({{
      type: "key_storage_probe",
      path: this.db_name,
      basename: basename(this.db_name),
      db: ptrString(this.db),
      key_len_arg: this.key_len_arg,
      retval: retval.toInt32(),
      before: this.before,
      after: after
	    }});
	  }}
	}});
	"###
    )
}

fn pcqq_key_storage_probe_script_for_run(
    runner: &str,
    events_path: &Path,
    account: &str,
) -> String {
    let event_path = process_path_for_runner(runner, events_path);
    let event_path_json = serde_json::to_string(&event_path).unwrap_or_else(|_| "null".to_string());
    let script = pcqq_key_storage_probe_script(account);
    let script = script.replace("send(", "qqAnalyzerEmit(");
    format!(
        r#"
const QQ_ANALYZER_EVENT_PATH = {event_path_json};
let qqAnalyzerEventFile = null;
function qqAnalyzerEmit(obj) {{
  try {{
    if (QQ_ANALYZER_EVENT_PATH && qqAnalyzerEventFile === null) {{
      qqAnalyzerEventFile = new File(QQ_ANALYZER_EVENT_PATH, "a");
    }}
    if (qqAnalyzerEventFile !== null) {{
      qqAnalyzerEventFile.write(JSON.stringify(obj) + "\n");
      qqAnalyzerEventFile.flush();
    }}
  }} catch (e) {{
  }}
  send(obj);
}}
{script}
"#
    )
}

fn rekey_result_from_events(path: &Path) -> anyhow::Result<Option<Value>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    for line in text.lines().rev() {
        let Ok(event) = parse_event_line(line.trim()) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("rekey_result") {
            return Ok(Some(event));
        }
    }
    Ok(None)
}

fn pcqq_query_result_from_events(path: &Path) -> anyhow::Result<Option<Value>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    for line in text.lines().rev() {
        let Ok(event) = parse_event_line(line.trim()) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("pcqq_query_result") {
            return Ok(Some(event));
        }
    }
    Ok(None)
}

fn count_event_lines(path: &Path) -> anyhow::Result<usize> {
    if !path.is_file() {
        return Ok(0);
    }
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    Ok(reader.lines().map_while(Result::ok).count())
}

fn latest_frida_version() -> anyhow::Result<String> {
    let value: Value = ureq::get("https://api.github.com/repos/frida/frida/releases/latest")
        .set("User-Agent", "qq-analyzer-rs")
        .call()?
        .into_json()?;
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("GitHub latest release response has no tag_name"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

fn download_to_file(url: &str, path: &Path) -> anyhow::Result<()> {
    let mut response = ureq::get(url)
        .set("User-Agent", "qq-analyzer-rs")
        .call()
        .map_err(|err| anyhow::anyhow!("download failed for {url}: {err}"))?
        .into_reader();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("download");
    let mut out = fs::File::create(&tmp)?;
    std::io::copy(&mut response, &mut out)?;
    out.flush()?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn make_executable(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(perms.mode() | 0o755);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn decompress_xz_file(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let input = fs::File::open(src)?;
    let mut decoder = XzDecoder::new(input);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_extension("extracting");
    let mut out = fs::File::create(&tmp)?;
    let mut buf = Vec::new();
    decoder.read_to_end(&mut buf)?;
    out.write_all(&buf)?;
    out.flush()?;
    fs::rename(tmp, dst)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dst)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dst, perms)?;
    }
    Ok(())
}

pub fn normalize_event_log(
    account: &str,
    input: &Path,
    credentials_path: &Path,
) -> anyhow::Result<NormalizeSummary> {
    let existing = if credentials_path.is_file() {
        credentials::read_credentials(credentials_path)?
    } else {
        Vec::new()
    };
    let mut known = existing
        .into_iter()
        .map(credential_identity)
        .collect::<std::collections::BTreeSet<_>>();
    let mut summary = NormalizeSummary {
        input: input.display().to_string(),
        credentials: credentials_path.display().to_string(),
        ..NormalizeSummary::default()
    };
    let text = fs::read_to_string(input)?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        summary.events += 1;
        let event = match parse_event_line(trimmed) {
            Ok(event) => event,
            Err(_) => {
                summary.ignored += 1;
                continue;
            }
        };
        if let Some(record) = event_to_credential(account, &event)? {
            let record = record.normalized();
            let identity = credential_identity(record.clone());
            if known.insert(identity) {
                credentials::append_credential(credentials_path, record)?;
                summary.imported += 1;
            } else {
                summary.ignored += 1;
            }
        } else {
            summary.ignored += 1;
        }
    }
    Ok(summary)
}

struct EventWriter {
    file: fs::File,
    events: usize,
}

fn spawn_event_reader<R>(
    stream: &'static str,
    reader: R,
    writer: Arc<Mutex<EventWriter>>,
) -> thread::JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines().map_while(Result::ok) {
            let event = extract_frida_event_json(&line).unwrap_or_else(|| {
                json!({
                    "type": "_raw",
                    "stream": stream,
                    "line": line,
                })
                .to_string()
            });
            if let Ok(mut writer) = writer.lock() {
                if writeln!(writer.file, "{event}").is_ok() {
                    writer.events += 1;
                }
            }
        }
    })
}

pub fn extract_frida_event_json(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_capture_event_json(trimmed) {
        return Some(trimmed.to_string());
    }
    for (idx, _) in trimmed.match_indices('{') {
        let candidate = &trimmed[idx..];
        if is_capture_event_json(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn is_capture_event_json(value: &str) -> bool {
    serde_json::from_str::<Value>(value)
        .ok()
        .is_some_and(|json| {
            let payload = json.get("payload").unwrap_or(&json);
            payload.get("type").and_then(Value::as_str).is_some()
        })
}

fn parse_event_line(line: &str) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_str(line)?;
    if let Some(payload) = value.get("payload") {
        Ok(payload.clone())
    } else {
        Ok(value)
    }
}

fn event_to_credential(account: &str, event: &Value) -> anyhow::Result<Option<CredentialRecord>> {
    let typ = event.get("type").and_then(Value::as_str).unwrap_or("");
    match typ {
        "infostorage_key" => {
            let Some(key_hex) = event.get("key_hex").and_then(Value::as_str) else {
                return Ok(None);
            };
            let mut metadata = std::collections::BTreeMap::new();
            for key in ["path", "source", "offset", "manager", "list_ptr"] {
                if let Some(value) = event.get(key).and_then(Value::as_str) {
                    metadata.insert(key.to_string(), value.to_string());
                }
            }
            Ok(Some(CredentialRecord {
                kind: CredentialKind::InfostorageTeaKey,
                account: account.to_string(),
                source: event
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("frida_common_init_storage_file_system")
                    .to_string(),
                db_path: event
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                key_hex: key_hex.to_string(),
                key_len: event
                    .get("key_len_assumed")
                    .or_else(|| event.get("keyLen"))
                    .and_then(Value::as_u64)
                    .unwrap_or(16) as usize,
                captured_at: String::new(),
                metadata,
            }))
        }
        "db" => {
            let Some(key_hex) = event
                .get("keyHex")
                .and_then(Value::as_str)
                .or_else(|| nested_str(event, &["state", "codec", "key_hex"]))
                .or_else(|| nested_str(event, &["state", "codec", "rekey_hex"]))
            else {
                return Ok(None);
            };
            let path = event.get("path").and_then(Value::as_str).unwrap_or("");
            let mut metadata = std::collections::BTreeMap::new();
            if let Some(basename) = event.get("basename").and_then(Value::as_str) {
                metadata.insert("basename".to_string(), basename.to_string());
            }
            for key in ["db", "cpp", "conn"] {
                if let Some(value) = event.get(key).and_then(Value::as_str) {
                    metadata.insert(key.to_string(), value.to_string());
                }
            }
            Ok(Some(CredentialRecord {
                kind: CredentialKind::PcqqSqliteKey,
                account: account.to_string(),
                source: if event.get("keyHex").is_some() {
                    "frida_kernelutil_key_db"
                } else {
                    "frida_kernelutil_multisqlite_memory"
                }
                .to_string(),
                db_path: path.to_string(),
                key_hex: key_hex.to_string(),
                key_len: event.get("keyLen").and_then(Value::as_u64).unwrap_or(16) as usize,
                captured_at: String::new(),
                metadata,
            }))
        }
        "key_storage_multisqlite_db" => {
            pcqq_sqlite_memory_event_to_credential(account, event, "codec")
        }
        "key_storage_probe" => pcqq_sqlite_memory_event_to_credential(account, event, "after"),
        "pcqq_sqlite_memory_key" => {
            let Some(key_hex) = event.get("key_hex").and_then(Value::as_str) else {
                return Ok(None);
            };
            let path = event.get("path").and_then(Value::as_str).unwrap_or("");
            let mut metadata = std::collections::BTreeMap::new();
            for key in [
                "basename",
                "db",
                "inner_db",
                "connection",
                "sqlite_db",
                "codec_state",
            ] {
                if let Some(value) = event.get(key).and_then(Value::as_str) {
                    metadata.insert(key.to_string(), value.to_string());
                }
            }
            Ok(Some(CredentialRecord {
                kind: CredentialKind::PcqqSqliteKey,
                account: account.to_string(),
                source: "frida_kernelutil_multisqlite_memory".to_string(),
                db_path: path.to_string(),
                key_hex: key_hex.to_string(),
                key_len: key_hex.len() / 2,
                captured_at: String::new(),
                metadata,
            }))
        }
        _ => Ok(None),
    }
}

fn pcqq_sqlite_memory_event_to_credential(
    account: &str,
    event: &Value,
    codec_root: &str,
) -> anyhow::Result<Option<CredentialRecord>> {
    let Some(key_hex) = nested_str(event, &[codec_root, "codec", "key_hex"])
        .or_else(|| nested_str(event, &[codec_root, "codec", "rekey_hex"]))
    else {
        return Ok(None);
    };
    let path = event.get("path").and_then(Value::as_str).unwrap_or("");
    let mut metadata = std::collections::BTreeMap::new();
    for key in [
        "basename",
        "db",
        "inner_db",
        "connection",
        "manager",
        "entry",
        "sqlite_db",
        "sqlite3",
    ] {
        if let Some(value) = event.get(key).and_then(Value::as_str) {
            metadata.insert(key.to_string(), value.to_string());
        }
    }
    for (metadata_key, path) in [
        ("codec_state", &[codec_root, "codec_state"][..]),
        ("sqlite_db", &[codec_root, "sqlite_db"][..]),
        ("connection", &[codec_root, "connection"][..]),
        ("table", &[codec_root, "table"][..]),
        ("sqlite_db_ptr_ptr", &[codec_root, "sqlite_db_ptr_ptr"][..]),
    ] {
        if let Some(value) = nested_str(event, path) {
            metadata.insert(metadata_key.to_string(), value.to_string());
        }
    }
    Ok(Some(CredentialRecord {
        kind: CredentialKind::PcqqSqliteKey,
        account: account.to_string(),
        source: if codec_root == "codec" {
            "frida_kernelutil_multisqlite_memory".to_string()
        } else {
            format!("frida_kernelutil_{codec_root}_memory")
        },
        db_path: path.to_string(),
        key_hex: key_hex.to_string(),
        key_len: key_hex.len() / 2,
        captured_at: String::new(),
        metadata,
    }))
}

fn nested_str<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    cursor.as_str()
}

fn credential_identity(
    record: CredentialRecord,
) -> (CredentialKind, String, String, String, String) {
    (
        record.kind,
        record.account,
        record.source,
        record.db_path,
        record.key_hex,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_frida_send_json() {
        let line = r#"{"type":"send","payload":{"type":"db","keyHex":"aabb","keyLen":1}}"#;
        assert_eq!(extract_frida_event_json(line), Some(line.to_string()));
    }

    #[test]
    fn extracts_prefixed_frida_send_json() {
        let line = r#"noise {"payload":{"type":"infostorage_key","key_hex":"aabb"}}"#;
        assert_eq!(
            extract_frida_event_json(line),
            Some(r#"{"payload":{"type":"infostorage_key","key_hex":"aabb"}}"#.to_string())
        );
    }

    #[test]
    fn ignores_non_event_json() {
        assert_eq!(extract_frida_event_json(r#"{"hello":"world"}"#), None);
    }

    #[test]
    fn converts_wsl_mount_path_to_windows_form() {
        let input = ["/mnt", "/z/example/db.sqlite"].concat();
        assert_eq!(
            wsl_path_to_windows(Path::new(&input)),
            "Z:\\example\\db.sqlite"
        );
    }

    #[test]
    fn leaves_non_wsl_mount_path_unchanged() {
        assert_eq!(
            wsl_path_to_windows(Path::new("/home/user/db.sqlite")),
            "/home/user/db.sqlite"
        );
    }
}

const INFOSTORAGE_HOOK_JS: &str = r#"
function buf2hex(buffer) {
  const byteArray = new Uint8Array(buffer);
  const parts = [];
  for (let i = 0; i < byteArray.length; i++) parts.push(("00" + byteArray[i].toString(16)).slice(-2));
  return parts.join("");
}
function readUtf16(ptr) {
  if (ptr.isNull()) return null;
  try { return ptr.readUtf16String(); } catch (e) { return "<readUtf16String failed: " + e + ">"; }
}
const exportName = "?InitStorageFileSystem@InfoStorage@Util@@YAJPA_WPAE@Z";
function findExport(moduleName, name) {
  if (Module.findExportByName) return Module.findExportByName(moduleName, name);
  try {
    const m = Process.getModuleByName(moduleName);
    if (m.findExportByName) return m.findExportByName(name);
    if (Module.getExportByName) return Module.getExportByName(moduleName, name);
  } catch (e) {
    return null;
  }
  return null;
}
function readU32(ptr) {
  return ptr.readU32() >>> 0;
}
function findManagerGlobal(initFunc) {
  const bytes = new Uint8Array(initFunc.readByteArray(96));
  const candidates = [];
  for (let i = 0; i + 6 <= bytes.length; i++) {
    if (bytes[i] === 0x83 && bytes[i + 1] === 0x3d && bytes[i + 6] === 0x00) candidates.push(ptr(readU32(initFunc.add(i + 2))));
    if ((bytes[i] === 0xa1 || (bytes[i] === 0x8b && (bytes[i + 1] & 0xc7) === 0x05)) && i + 6 <= bytes.length) {
      const off = bytes[i] === 0xa1 ? i + 1 : i + 2;
      candidates.push(ptr(readU32(initFunc.add(off))));
    }
  }
  return candidates;
}
let target = findExport("Common.dll", exportName);
if (target === null) {
  try { Module.load("Common.dll"); } catch (e) {}
  target = findExport("Common.dll", exportName);
}
if (target === null) {
  send({type: "fatal", error: "Common.dll export not found", exportName: exportName});
} else {
  send({type: "hooked", module: "Common.dll", exportName: exportName, address: target.toString()});
  try {
    const common = Process.getModuleByName("Common.dll");
    const globals = findManagerGlobal(target);
    const managerGlobal = globals.length ? globals[0] : common.base.add(0x28aeb0);
    send({type: "manager_global_candidate", address: managerGlobal.toString(), candidates: globals.map(String)});
    const manager = managerGlobal.readPointer();
    send({type: "manager_pointer", manager: manager.toString()});
    if (!manager.isNull()) {
      let managerPath = null;
      try {
        const pathPtr = manager.add(0x1c).readPointer();
        managerPath = readUtf16(pathPtr);
      } catch (e) {
        managerPath = "<manager path read failed: " + e + ">";
      }
      const derive = new NativeFunction(common.base.add(0xd1ba0), "void", ["pointer", "pointer"], "thiscall");
      for (const off of [0x2c, 0x3c]) {
        const listPtr = manager.add(off).readPointer();
        send({type: "manager_keylist_candidate", offset: "0x" + off.toString(16), list_ptr: listPtr.toString(), manager_path: managerPath});
        const out = Memory.alloc(16);
        if (!listPtr.isNull()) {
          derive(listPtr, out);
          send({type: "infostorage_key", source: "existing_manager", offset: "0x" + off.toString(16), manager: manager.toString(), path: managerPath, list_ptr: listPtr.toString(), key_len_assumed: 16, key_hex: buf2hex(out.readByteArray(16))});
        }
      }
    }
  } catch (e) {
    send({type: "manager_read_error", error: String(e)});
  }
  const seen = {};
  Interceptor.attach(target, {
    onEnter: function(args) {
      const path = readUtf16(args[0]);
      let keyHex = null;
      try { if (!args[1].isNull()) keyHex = buf2hex(args[1].readByteArray(16)); } catch (e) { keyHex = "<read key failed: " + e + ">"; }
      const dedupe = String(path) + "|" + String(keyHex);
      if (seen[dedupe]) return;
      seen[dedupe] = true;
      send({type: "infostorage_key", path: path, key_len_assumed: 16, key_hex: keyHex});
    }
  });
}
"#;

const PCQQ_SQLITE_HOOK_JS: &str = r###"
function buf2hex(buffer) {
  const byteArray = new Uint8Array(buffer);
  const hexParts = [];
  for (let i = 0; i < byteArray.length; i++) hexParts.push(("00" + byteArray[i].toString(16)).slice(-2));
  return hexParts.join("");
}
const QQ_ANALYZER_ACCOUNT = "__QQ_ANALYZER_ACCOUNT__";
const kernel_util = Module.load('KernelUtil.dll');
function single_function(pattern) {
  pattern = pattern.replaceAll("##", "").replaceAll(" ", "").toLowerCase().replace(/\s/g,'').replace(/(.{2})/g,"$1 ");
  var hits = Memory.scanSync(kernel_util.base, kernel_util.size, pattern);
  if (hits.length > 1) {
    send({type: "fatal", error: "pattern found multiple", pattern: pattern, count: hits.length});
    throw new Error("pattern found multiple");
  }
  if (hits.length == 0) {
    send({type: "fatal", error: "pattern not found", pattern: pattern});
    throw new Error("pattern not found");
  }
  return hits[0].address;
}
const key_function = single_function("55 8b ec 56 6b 75 10 11 83 7d 10 10 74 0d 68 17 02 00 00 e8");
const name_function = single_function("55 8B EC FF 75 0C FF 75 08 E8 B8 D1 02 00 59 59 85");
const name_db = new NativeFunction(name_function, 'pointer', ['pointer', 'pointer']);
send({
  type: "hooked",
  module: "KernelUtil.dll",
  hook: "pcqq_sqlite_key",
  account: QQ_ANALYZER_ACCOUNT,
  key_function: key_function.toString(),
  name_function: name_function.toString()
});
const seen = {};
function basename(p) {
  return p.replaceAll('/', '\\').split('\\').pop().toLowerCase();
}
function shouldHandle(p, keyLen) {
  const n = p.replaceAll('/', '\\').toLowerCase();
  if (keyLen !== 16) return false;
  if (!n.endsWith(".db")) return false;
  if (QQ_ANALYZER_ACCOUNT && n.indexOf("\\" + QQ_ANALYZER_ACCOUNT.toLowerCase() + "\\") < 0) return false;
  if (n.indexOf("\\nt_qq\\") >= 0 || n.indexOf("\\nt_db\\") >= 0 || n.indexOf("\\nt_data\\") >= 0) return false;
  return true;
}
Interceptor.attach(key_function, {
  onEnter: function(args) {
    let dbName = null;
    try { dbName = name_db(args[0], NULL).readUtf8String(); } catch (e) { return; }
    const keyLen = args[2].toInt32();
    if (!shouldHandle(dbName, keyLen)) return;
    const keyHex = buf2hex(args[1].readByteArray(keyLen));
    const key = dbName.toLowerCase();
    if (seen[key]) return;
    seen[key] = true;
    send({type: "db", path: dbName, basename: basename(dbName), keyLen: keyLen, keyHex: keyHex});
  }
});
"###;

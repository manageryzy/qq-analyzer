use std::collections::BTreeMap;
#[cfg(windows)]
use std::collections::VecDeque;
use std::env;
#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle};

use anyhow::Context;
use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crossbeam_channel::{bounded, unbounded, Receiver, TryRecvError};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    BuildIoRingReadFile, CloseIoRing, CreateIoRing, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, IsIoRingOpSupported,
    PopIoRingCompletion, QueryIoRingCapabilities, SubmitIoRing, FILE_ATTRIBUTE_DIRECTORY,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH, HIORING,
    IORING_BUFFER_REF, IORING_BUFFER_REF_0, IORING_CAPABILITIES, IORING_CQE,
    IORING_CREATE_ADVISORY_FLAGS_NONE, IORING_CREATE_FLAGS, IORING_CREATE_REQUIRED_FLAGS_NONE,
    IORING_HANDLE_REF, IORING_HANDLE_REF_0, IORING_OP_READ, IORING_REF_RAW, IORING_VERSION_1,
    IORING_VERSION_2, IORING_VERSION_3, IOSQE_FLAGS_NONE, SYNCHRONIZE, WIN32_FIND_DATAW,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

const PROBE_VERSION: &str = "media_info_v1_header_container";
const MAX_PROBE_READ_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_PROBE_READ_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_MP4_TAIL_READ_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_SQLITE_WRITE_BATCH_SIZE: usize = 1000;
const DEFAULT_PROGRESS_INTERVAL_SECS: u64 = 10;
const DEFAULT_WALK_EVENT_QUEUE_SIZE: usize = 8192;
const DEFAULT_PROBE_JOB_BATCH_SIZE: usize = 32;
const MAX_DETAILS_JSON_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_FILES: usize = 200_000;
const DEFAULT_WORKERS_CAP: usize = 12;
#[cfg(windows)]
const IORING_WAIT_INFINITE_MS: u32 = u32::MAX;
#[cfg(windows)]
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
#[cfg(windows)]
const NT_FILE_OPEN: u32 = 0x01;
#[cfg(windows)]
const NT_FILE_NON_DIRECTORY_FILE: u32 = 0x40;
#[cfg(windows)]
const NT_FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HashMode {
    None,
    Full,
}

impl HashMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "off" | "no" => Ok(Self::None),
            "full" | "sha256" | "sha256-full" => Ok(Self::Full),
            _ => anyhow::bail!("unknown hash mode: {value}; expected none or full"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Jsonl,
    Csv,
}

impl ExportFormat {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "jsonl" | "json-lines" | "ndjson" => Ok(Self::Jsonl),
            "csv" => Ok(Self::Csv),
            _ => anyhow::bail!("unknown export format: {value}; expected jsonl or csv"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Csv => "csv",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub root: PathBuf,
    pub account: String,
    pub asset_roots: Vec<PathBuf>,
    pub sqlite_path: Option<PathBuf>,
    pub max_files: usize,
    pub workers: usize,
    pub force: bool,
    pub hash_mode: HashMode,
}

impl BuildOptions {
    fn normalized(mut self) -> Self {
        if self.max_files == 0 {
            self.max_files = DEFAULT_MAX_FILES;
        }
        if self.workers == 0 {
            self.workers = default_workers();
        }
        self.workers = self.workers.clamp(1, 64);
        self
    }
}

#[derive(Clone, Debug)]
pub struct StatusOptions {
    pub root: PathBuf,
    pub account: String,
    pub sqlite_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct RepairOptions {
    pub root: PathBuf,
    pub account: String,
    pub sqlite_path: Option<PathBuf>,
    pub max_files: usize,
    pub workers: usize,
    pub hash_mode: HashMode,
}

impl RepairOptions {
    fn normalized(mut self) -> Self {
        if self.max_files == 0 {
            self.max_files = usize::MAX;
        }
        if self.workers == 0 {
            self.workers = default_workers();
        }
        self.workers = self.workers.clamp(1, 64);
        self
    }
}

#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub sqlite_path: PathBuf,
    pub format: ExportFormat,
    pub out: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct BuildReport {
    pub account: String,
    pub sqlite_path: String,
    pub roots: Vec<String>,
    pub root_stats: Vec<RootScanReport>,
    pub files_seen: usize,
    pub scanned_files: usize,
    pub indexed_files: usize,
    pub unchanged_files: usize,
    pub unsupported_files: usize,
    pub partial_files: usize,
    pub error_files: usize,
    pub walk_errors: usize,
    pub max_files: usize,
    pub workers: usize,
    pub walkers: usize,
    pub hash_mode: String,
    pub by_media_kind: BTreeMap<String, usize>,
    pub by_container: BTreeMap<String, usize>,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<BuildProfileReport>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BuildProfileReport {
    pub enumerate_ms: u64,
    pub walk_next_ms: u64,
    pub skip_query_ms: u64,
    pub probe_ms: u64,
    pub worker_wait_ms: u64,
    pub open_ms: u64,
    pub read_ms: u64,
    pub parse_ms: u64,
    pub close_ms: u64,
    pub sqlite_ms: u64,
    pub probe_open_ms: u64,
    pub probe_read_ms: u64,
    pub probe_hash_ms: u64,
    pub probe_parse_ms: u64,
    pub probe_close_ms: u64,
    pub probe_bytes_read: u64,
    pub probe_short_reads: usize,
    pub relative_open_files: usize,
    pub dir_cache_hits: usize,
    pub dir_cache_misses: usize,
    pub queue_send_ms: u64,
    pub result_receive_ms: u64,
    pub sqlite_flush_ms: u64,
    pub sqlite_upsert_ms: u64,
    pub sqlite_delete_ms: u64,
    pub sqlite_stream_insert_ms: u64,
    pub sqlite_feature_insert_ms: u64,
    pub sqlite_commit_ms: u64,
    pub sqlite_flushes: usize,
    pub sqlite_rows: usize,
    pub max_inflight_jobs: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RootScanReport {
    pub root: String,
    pub directories_seen: usize,
    pub files_seen: usize,
    pub scanned_files: usize,
    pub indexed_files: usize,
    pub unchanged_files: usize,
    pub unsupported_files: usize,
    pub partial_files: usize,
    pub error_files: usize,
    pub walk_errors: usize,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct InspectReport {
    pub input: String,
    pub file: MediaFileRecord,
    pub streams: Vec<MediaStreamRecord>,
    pub codec_features: Vec<MediaCodecFeatureRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusReport {
    pub sqlite_path: String,
    pub exists: bool,
    pub total_files: i64,
    pub by_media_kind: BTreeMap<String, i64>,
    pub by_container: BTreeMap<String, i64>,
    pub by_status: BTreeMap<String, i64>,
    pub errors: i64,
    pub partials: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepairReport {
    pub account: String,
    pub sqlite_path: String,
    pub candidate_rows: usize,
    pub missing_files: usize,
    pub repaired_files: usize,
    pub changed_files: usize,
    pub fixed_files: usize,
    pub still_problematic_files: usize,
    pub unsupported_files: usize,
    pub partial_files: usize,
    pub error_files: usize,
    pub max_files: usize,
    pub workers: usize,
    pub hash_mode: String,
    pub by_media_kind: BTreeMap<String, usize>,
    pub by_container: BTreeMap<String, usize>,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<BuildProfileReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExportReport {
    pub sqlite_path: String,
    pub out: String,
    pub format: String,
    pub rows: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaProbe {
    pub file: MediaFileRecord,
    pub streams: Vec<MediaStreamRecord>,
    pub codec_features: Vec<MediaCodecFeatureRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaFileRecord {
    pub path: String,
    pub source_root: Option<String>,
    pub relative_path: Option<String>,
    pub file_name: String,
    pub extension: Option<String>,
    pub file_size: i64,
    pub mtime_unix: i64,
    pub media_kind: String,
    pub container: String,
    pub detected_format: String,
    pub mime: Option<String>,
    pub hash_mode: String,
    pub sha256_hex: Option<String>,
    pub probe_version: String,
    pub scan_status: String,
    pub error: Option<String>,
    pub scanned_at_unix: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MediaStreamRecord {
    pub stream_index: i64,
    pub stream_type: String,
    pub codec: Option<String>,
    pub codec_tag: Option<String>,
    pub profile: Option<String>,
    pub level: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub coded_width: Option<i64>,
    pub coded_height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub bitrate: Option<i64>,
    pub fps_num: Option<i64>,
    pub fps_den: Option<i64>,
    pub sample_rate: Option<i64>,
    pub channels: Option<i64>,
    pub bit_depth: Option<i64>,
    pub pixel_format: Option<String>,
    pub chroma_subsampling: Option<String>,
    pub color_primaries: Option<String>,
    pub transfer_characteristics: Option<String>,
    pub matrix_coefficients: Option<String>,
    pub rotation_degrees: Option<i64>,
    pub details_json: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MediaCodecFeatureRecord {
    pub stream_index: i64,
    pub standard: Option<String>,
    pub coding_mode: Option<String>,
    pub transform: Option<String>,
    pub prediction: Option<String>,
    pub entropy_coding: Option<String>,
    pub progressive: Option<String>,
    pub lossless: Option<i64>,
    pub near_lossless: Option<i64>,
    pub alpha: Option<i64>,
    pub animation: Option<i64>,
    pub icc_profile: Option<i64>,
    pub exif: Option<i64>,
    pub xmp: Option<i64>,
    pub orientation: Option<i64>,
    pub confidence: Option<String>,
    pub features_json: Option<String>,
}

struct ProbeJob {
    root_index: usize,
    source_root: PathBuf,
    entries: Vec<MediaFileEntry>,
}

struct RepairJob {
    targets: Vec<RepairTarget>,
}

#[derive(Clone)]
struct WalkDirJob {
    root_index: usize,
    source_root: PathBuf,
    dir: PathBuf,
}

enum WalkEvent {
    Stats {
        root_index: usize,
        directories_seen: usize,
        errors: usize,
        enumerate_elapsed: Duration,
    },
    Entries {
        root_index: usize,
        source_root: PathBuf,
        entries: Vec<MediaFileEntry>,
    },
}

struct ProbeResult {
    root_index: usize,
    probe: MediaProbe,
    probe_elapsed: Duration,
    worker_wait_elapsed: Duration,
    probe_timing: ProbeTiming,
}

struct RepairProbeResult {
    old: RepairOldClass,
    result: ProbeResult,
}

#[derive(Clone, Debug)]
struct RepairTarget {
    entry: MediaFileEntry,
    source_root: Option<PathBuf>,
    old: RepairOldClass,
}

#[derive(Clone, Debug)]
struct RepairOldClass {
    media_kind: String,
    container: String,
    detected_format: String,
    mime: Option<String>,
    scan_status: String,
    error: Option<String>,
}

#[derive(Default)]
struct RepairCounters {
    changed_files: usize,
    fixed_files: usize,
    still_problematic_files: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProbeTiming {
    open: Duration,
    read: Duration,
    hash: Duration,
    parse: Duration,
    close: Duration,
    bytes_read: u64,
    short_reads: usize,
    relative_open_files: usize,
    dir_cache_hits: usize,
    dir_cache_misses: usize,
}

struct TailProbe {
    offset: u64,
    data: Vec<u8>,
}

#[cfg(windows)]
struct WindowsIoRingReader {
    ring: HIORING,
}

#[cfg(windows)]
struct WindowsProbeContext {
    io_ring_reader: Option<WindowsIoRingReader>,
    relative_open_cache: Option<WindowsRelativeOpenCache>,
}

#[cfg(windows)]
struct WindowsRelativeOpenCache {
    capacity: usize,
    dirs: VecDeque<WindowsCachedDir>,
}

#[cfg(windows)]
struct WindowsCachedDir {
    path: PathBuf,
    handle: File,
}

#[cfg(windows)]
#[repr(C)]
struct NtObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *const UNICODE_STRING,
    attributes: u32,
    security_descriptor: *mut core::ffi::c_void,
    security_quality_of_service: *mut core::ffi::c_void,
}

#[cfg(windows)]
#[link(name = "ntdll")]
extern "system" {
    fn NtCreateFile(
        file_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const NtObjectAttributes,
        io_status_block: *mut IO_STATUS_BLOCK,
        allocation_size: *mut i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *mut core::ffi::c_void,
        ea_length: u32,
    ) -> i32;
}

#[cfg(not(windows))]
struct WindowsProbeContext;

#[derive(Clone, Copy)]
enum ProfileStage {
    Enumerate,
    WalkNext,
    SkipQuery,
    Probe,
    WorkerWait,
    ProbeOpen,
    ProbeRead,
    ProbeHash,
    ProbeParse,
    ProbeClose,
    QueueSend,
    ResultReceive,
    SqliteFlush,
    SqliteUpsert,
    SqliteDelete,
    SqliteStreamInsert,
    SqliteFeatureInsert,
    SqliteCommit,
}

#[derive(Default)]
struct BuildProfiler {
    enabled: bool,
    enumerate: Duration,
    walk_next: Duration,
    skip_query: Duration,
    probe: Duration,
    worker_wait: Duration,
    probe_open: Duration,
    probe_read: Duration,
    probe_hash: Duration,
    probe_parse: Duration,
    probe_close: Duration,
    probe_bytes_read: u64,
    probe_short_reads: usize,
    relative_open_files: usize,
    dir_cache_hits: usize,
    dir_cache_misses: usize,
    queue_send: Duration,
    result_receive: Duration,
    sqlite_flush: Duration,
    sqlite_upsert: Duration,
    sqlite_delete: Duration,
    sqlite_stream_insert: Duration,
    sqlite_feature_insert: Duration,
    sqlite_commit: Duration,
    sqlite_flushes: usize,
    sqlite_rows: usize,
    max_inflight_jobs: usize,
}

impl BuildProfiler {
    fn from_env() -> Self {
        Self {
            enabled: env_flag("QQ_ANALYZER_MEDIA_PROFILE"),
            ..Self::default()
        }
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn time<T>(&mut self, stage: ProfileStage, op: impl FnOnce() -> T) -> T {
        if !self.enabled {
            return op();
        }
        let started = Instant::now();
        let value = op();
        self.add_duration(stage, started.elapsed());
        value
    }

    fn add_duration(&mut self, stage: ProfileStage, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        match stage {
            ProfileStage::Enumerate => self.enumerate += elapsed,
            ProfileStage::WalkNext => self.walk_next += elapsed,
            ProfileStage::SkipQuery => self.skip_query += elapsed,
            ProfileStage::Probe => self.probe += elapsed,
            ProfileStage::WorkerWait => self.worker_wait += elapsed,
            ProfileStage::ProbeOpen => self.probe_open += elapsed,
            ProfileStage::ProbeRead => self.probe_read += elapsed,
            ProfileStage::ProbeHash => self.probe_hash += elapsed,
            ProfileStage::ProbeParse => self.probe_parse += elapsed,
            ProfileStage::ProbeClose => self.probe_close += elapsed,
            ProfileStage::QueueSend => self.queue_send += elapsed,
            ProfileStage::ResultReceive => self.result_receive += elapsed,
            ProfileStage::SqliteFlush => self.sqlite_flush += elapsed,
            ProfileStage::SqliteUpsert => self.sqlite_upsert += elapsed,
            ProfileStage::SqliteDelete => self.sqlite_delete += elapsed,
            ProfileStage::SqliteStreamInsert => self.sqlite_stream_insert += elapsed,
            ProfileStage::SqliteFeatureInsert => self.sqlite_feature_insert += elapsed,
            ProfileStage::SqliteCommit => self.sqlite_commit += elapsed,
        }
    }

    fn add_probe_timing(&mut self, timing: ProbeTiming) {
        self.add_duration(ProfileStage::ProbeOpen, timing.open);
        self.add_duration(ProfileStage::ProbeRead, timing.read);
        self.add_duration(ProfileStage::ProbeHash, timing.hash);
        self.add_duration(ProfileStage::ProbeParse, timing.parse);
        self.add_duration(ProfileStage::ProbeClose, timing.close);
        if self.enabled {
            self.probe_bytes_read = self.probe_bytes_read.saturating_add(timing.bytes_read);
            self.probe_short_reads = self.probe_short_reads.saturating_add(timing.short_reads);
            self.relative_open_files = self
                .relative_open_files
                .saturating_add(timing.relative_open_files);
            self.dir_cache_hits = self.dir_cache_hits.saturating_add(timing.dir_cache_hits);
            self.dir_cache_misses = self
                .dir_cache_misses
                .saturating_add(timing.dir_cache_misses);
        }
    }

    fn record_inflight(&mut self, inflight: usize) {
        if self.enabled {
            self.max_inflight_jobs = self.max_inflight_jobs.max(inflight);
        }
    }

    fn record_sqlite_flush(&mut self, rows: usize) {
        if self.enabled {
            self.sqlite_flushes += 1;
            self.sqlite_rows += rows;
        }
    }

    fn report(&self) -> Option<BuildProfileReport> {
        self.enabled.then(|| BuildProfileReport {
            enumerate_ms: duration_ms(self.enumerate),
            walk_next_ms: duration_ms(self.walk_next),
            skip_query_ms: duration_ms(self.skip_query),
            probe_ms: duration_ms(self.probe),
            worker_wait_ms: duration_ms(self.worker_wait),
            open_ms: duration_ms(self.probe_open),
            read_ms: duration_ms(self.probe_read),
            parse_ms: duration_ms(self.probe_parse),
            close_ms: duration_ms(self.probe_close),
            sqlite_ms: duration_ms(self.sqlite_flush),
            probe_open_ms: duration_ms(self.probe_open),
            probe_read_ms: duration_ms(self.probe_read),
            probe_hash_ms: duration_ms(self.probe_hash),
            probe_parse_ms: duration_ms(self.probe_parse),
            probe_close_ms: duration_ms(self.probe_close),
            probe_bytes_read: self.probe_bytes_read,
            probe_short_reads: self.probe_short_reads,
            relative_open_files: self.relative_open_files,
            dir_cache_hits: self.dir_cache_hits,
            dir_cache_misses: self.dir_cache_misses,
            queue_send_ms: duration_ms(self.queue_send),
            result_receive_ms: duration_ms(self.result_receive),
            sqlite_flush_ms: duration_ms(self.sqlite_flush),
            sqlite_upsert_ms: duration_ms(self.sqlite_upsert),
            sqlite_delete_ms: duration_ms(self.sqlite_delete),
            sqlite_stream_insert_ms: duration_ms(self.sqlite_stream_insert),
            sqlite_feature_insert_ms: duration_ms(self.sqlite_feature_insert),
            sqlite_commit_ms: duration_ms(self.sqlite_commit),
            sqlite_flushes: self.sqlite_flushes,
            sqlite_rows: self.sqlite_rows,
            max_inflight_jobs: self.max_inflight_jobs,
        })
    }

    fn progress_fragment(&self) -> Option<String> {
        self.enabled.then(|| {
            format!(
                "profile_ms enum={} walk_wait={} skip={} probe={} worker_wait={} open={} read={} read_mb={} short_reads={} rel_open={} dir_hit={} dir_miss={} hash={} parse={} close={} send={} recv={} sqlite={} upsert={} delete={} stream={} feature={} commit={} flushes={} rows={} max_inflight={}",
                duration_ms(self.enumerate),
                duration_ms(self.walk_next),
                duration_ms(self.skip_query),
                duration_ms(self.probe),
                duration_ms(self.worker_wait),
                duration_ms(self.probe_open),
                duration_ms(self.probe_read),
                self.probe_bytes_read / (1024 * 1024),
                self.probe_short_reads,
                self.relative_open_files,
                self.dir_cache_hits,
                self.dir_cache_misses,
                duration_ms(self.probe_hash),
                duration_ms(self.probe_parse),
                duration_ms(self.probe_close),
                duration_ms(self.queue_send),
                duration_ms(self.result_receive),
                duration_ms(self.sqlite_flush),
                duration_ms(self.sqlite_upsert),
                duration_ms(self.sqlite_delete),
                duration_ms(self.sqlite_stream_insert),
                duration_ms(self.sqlite_feature_insert),
                duration_ms(self.sqlite_commit),
                self.sqlite_flushes,
                self.sqlite_rows,
                self.max_inflight_jobs
            )
        })
    }
}

struct ProgressReporter {
    interval: Option<Duration>,
    started: Instant,
    last_emit: Instant,
    emitted: bool,
    last_path: Option<String>,
}

impl ProgressReporter {
    fn from_env(started: Instant) -> Self {
        Self {
            interval: progress_interval(),
            started,
            last_emit: started,
            emitted: false,
            last_path: None,
        }
    }

    fn observe_path(&mut self, path: &Path) {
        self.last_path = Some(truncate_progress_path(&path_string(path)));
    }

    fn maybe_emit(
        &mut self,
        report: &BuildReport,
        pending_writes: usize,
        profiler: &BuildProfiler,
    ) {
        let Some(interval) = self.interval else {
            return;
        };
        if self.last_emit.elapsed() >= interval {
            self.emit("progress", report, pending_writes, profiler);
        }
    }

    fn finish(&mut self, report: &BuildReport, profiler: &BuildProfiler) {
        if self.emitted {
            self.emit("done", report, 0, profiler);
        }
    }

    fn emit(
        &mut self,
        label: &str,
        report: &BuildReport,
        pending_writes: usize,
        profiler: &BuildProfiler,
    ) {
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let scan_rate = report.scanned_files as f64 / elapsed;
        let index_rate = report.indexed_files as f64 / elapsed;
        let last_path = self.last_path.as_deref().unwrap_or("-");
        let profile = profiler
            .progress_fragment()
            .map(|value| format!(" {value}"))
            .unwrap_or_default();
        eprintln!(
            "[media-info] {label} elapsed={elapsed:.1}s seen={} scanned={}/{} indexed={} unchanged={} partial={} unsupported={} errors={} walk_errors={} pending_writes={} scan_rate={scan_rate:.1}/s indexed_rate={index_rate:.1}/s last=\"{}\"{}",
            report.files_seen,
            report.scanned_files,
            report.max_files,
            report.indexed_files,
            report.unchanged_files,
            report.partial_files,
            report.unsupported_files,
            report.error_files,
            report.walk_errors,
            pending_writes,
            last_path,
            profile
        );
        self.last_emit = Instant::now();
        self.emitted = true;
    }
}

pub fn default_sqlite_path(root: &Path, account: &str) -> PathBuf {
    config::account_output_root(root, account)
        .join("media-info")
        .join("media-info.sqlite")
}

pub fn build_index(options: BuildOptions) -> anyhow::Result<BuildReport> {
    let options = options.normalized();
    let started = Instant::now();
    let roots = scan_roots(&options);
    let sqlite_path = options
        .sqlite_path
        .clone()
        .unwrap_or_else(|| default_sqlite_path(&options.root, &options.account));
    if let Some(parent) = sqlite_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut con = Connection::open(&sqlite_path)?;
    configure_connection(&con)?;
    init_schema(&con)?;

    let mut report = BuildReport {
        account: options.account.clone(),
        sqlite_path: sqlite_path.display().to_string(),
        roots: roots
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        root_stats: roots
            .iter()
            .map(|root| RootScanReport {
                root: root.display().to_string(),
                ..RootScanReport::default()
            })
            .collect(),
        files_seen: 0,
        scanned_files: 0,
        indexed_files: 0,
        unchanged_files: 0,
        unsupported_files: 0,
        partial_files: 0,
        error_files: 0,
        walk_errors: 0,
        max_files: options.max_files,
        workers: options.workers,
        walkers: if options.workers <= 1 {
            1
        } else {
            default_walker_threads(options.workers)
        },
        hash_mode: options.hash_mode.as_str().to_string(),
        by_media_kind: BTreeMap::new(),
        by_container: BTreeMap::new(),
        elapsed_ms: 0,
        profile: None,
    };
    if roots.is_empty() {
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        return Ok(report);
    }

    let mut profiler = BuildProfiler::from_env();
    let mut progress = ProgressReporter::from_env(started);
    if options.workers <= 1 {
        build_serial(
            &mut con,
            &options,
            &roots,
            &mut report,
            &mut progress,
            &mut profiler,
        )?;
    } else {
        build_parallel(
            &mut con,
            &options,
            &roots,
            &mut report,
            &mut progress,
            &mut profiler,
        )?;
    }

    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report.profile = profiler.report();
    progress.finish(&report, &profiler);
    Ok(report)
}

pub fn inspect_file(input: &Path, hash_mode: HashMode) -> anyhow::Result<InspectReport> {
    let entry = media_file_entry_from_path(input)?;
    let probe = probe_entry(&entry, None, hash_mode)
        .with_context(|| format!("inspect {}", input.display()))?;
    Ok(InspectReport {
        input: input.display().to_string(),
        file: probe.file,
        streams: probe.streams,
        codec_features: probe.codec_features,
    })
}

pub fn status(options: StatusOptions) -> anyhow::Result<StatusReport> {
    let sqlite_path = options
        .sqlite_path
        .unwrap_or_else(|| default_sqlite_path(&options.root, &options.account));
    if !sqlite_path.is_file() {
        return Ok(StatusReport {
            sqlite_path: sqlite_path.display().to_string(),
            exists: false,
            total_files: 0,
            by_media_kind: BTreeMap::new(),
            by_container: BTreeMap::new(),
            by_status: BTreeMap::new(),
            errors: 0,
            partials: 0,
        });
    }
    let con = Connection::open(&sqlite_path)?;
    Ok(StatusReport {
        sqlite_path: sqlite_path.display().to_string(),
        exists: true,
        total_files: count_i64(&con, "SELECT count(*) FROM media_files")?,
        by_media_kind: grouped_counts(&con, "media_kind")?,
        by_container: grouped_counts(&con, "container")?,
        by_status: grouped_counts(&con, "scan_status")?,
        errors: count_i64(
            &con,
            "SELECT count(*) FROM media_files WHERE scan_status='error'",
        )?,
        partials: count_i64(
            &con,
            "SELECT count(*) FROM media_files WHERE scan_status='partial'",
        )?,
    })
}

pub fn repair_index(options: RepairOptions) -> anyhow::Result<RepairReport> {
    let options = options.normalized();
    let started = Instant::now();
    let sqlite_path = options
        .sqlite_path
        .clone()
        .unwrap_or_else(|| default_sqlite_path(&options.root, &options.account));
    let mut con = Connection::open(&sqlite_path)
        .with_context(|| format!("open {}", sqlite_path.display()))?;
    configure_connection(&con)?;
    init_schema(&con)?;

    let mut missing_files = 0usize;
    let targets = load_repair_targets(&con, options.max_files, &mut missing_files)?;
    let candidate_rows = targets.len().saturating_add(missing_files);
    let mut build_report = BuildReport {
        account: options.account.clone(),
        sqlite_path: sqlite_path.display().to_string(),
        roots: vec!["repair".to_string()],
        root_stats: vec![RootScanReport {
            root: "repair".to_string(),
            ..RootScanReport::default()
        }],
        files_seen: 0,
        scanned_files: 0,
        indexed_files: 0,
        unchanged_files: 0,
        unsupported_files: 0,
        partial_files: 0,
        error_files: 0,
        walk_errors: missing_files,
        max_files: options.max_files,
        workers: options.workers,
        walkers: 0,
        hash_mode: options.hash_mode.as_str().to_string(),
        by_media_kind: BTreeMap::new(),
        by_container: BTreeMap::new(),
        elapsed_ms: 0,
        profile: None,
    };
    build_report.root_stats[0].walk_errors = missing_files;

    let mut profiler = BuildProfiler::from_env();
    let mut progress = ProgressReporter::from_env(started);
    let mut counters = RepairCounters {
        still_problematic_files: missing_files,
        ..RepairCounters::default()
    };

    if !targets.is_empty() {
        if options.workers <= 1 {
            repair_serial(
                &mut con,
                &options,
                targets,
                &mut build_report,
                &mut counters,
                &mut progress,
                &mut profiler,
            )?;
        } else {
            repair_parallel(
                &mut con,
                &options,
                targets,
                &mut build_report,
                &mut counters,
                &mut progress,
                &mut profiler,
            )?;
        }
    }

    build_report.elapsed_ms = started.elapsed().as_millis() as u64;
    build_report.profile = profiler.report();
    progress.finish(&build_report, &profiler);

    Ok(RepairReport {
        account: options.account,
        sqlite_path: sqlite_path.display().to_string(),
        candidate_rows,
        missing_files,
        repaired_files: build_report.indexed_files,
        changed_files: counters.changed_files,
        fixed_files: counters.fixed_files,
        still_problematic_files: counters.still_problematic_files,
        unsupported_files: build_report.unsupported_files,
        partial_files: build_report.partial_files,
        error_files: build_report.error_files,
        max_files: build_report.max_files,
        workers: build_report.workers,
        hash_mode: build_report.hash_mode,
        by_media_kind: build_report.by_media_kind,
        by_container: build_report.by_container,
        elapsed_ms: build_report.elapsed_ms,
        profile: build_report.profile,
    })
}

pub fn export(options: ExportOptions) -> anyhow::Result<ExportReport> {
    let con = Connection::open(&options.sqlite_path)
        .with_context(|| format!("open {}", options.sqlite_path.display()))?;
    if let Some(parent) = options.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = File::create(&options.out)?;
    let mut rows = 0usize;
    match options.format {
        ExportFormat::Jsonl => {
            let mut stmt = con.prepare(
                "SELECT id, path, source_root, relative_path, file_name, extension, file_size,
                        mtime_unix, media_kind, container, detected_format, mime, hash_mode,
                        sha256_hex, probe_version, scan_status, error, scanned_at_unix
                 FROM media_files ORDER BY path",
            )?;
            let mapped = stmt.query_map([], media_file_row_from_sql)?;
            for row in mapped {
                let (id, file) = row?;
                let streams = load_streams(&con, id)?;
                let features = load_features(&con, id)?;
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string(&json!({
                        "file": file,
                        "streams": streams,
                        "codec_features": features,
                    }))?
                )?;
                rows += 1;
            }
        }
        ExportFormat::Csv => {
            writeln!(
                out,
                "path,media_kind,container,detected_format,mime,file_size,mtime_unix,scan_status,error,streams,sha256_hex"
            )?;
            let mut stmt = con.prepare(
                "SELECT f.id, f.path, f.media_kind, f.container, f.detected_format, f.mime,
                        f.file_size, f.mtime_unix, f.scan_status, f.error, f.sha256_hex,
                        count(s.stream_index) AS streams
                 FROM media_files f
                 LEFT JOIN media_streams s ON s.file_id=f.id
                 GROUP BY f.id
                 ORDER BY f.path",
            )?;
            let mut query = stmt.query([])?;
            while let Some(row) = query.next()? {
                let values = [
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    row.get::<_, i64>(6)?.to_string(),
                    row.get::<_, i64>(7)?.to_string(),
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                    row.get::<_, i64>(11)?.to_string(),
                    row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                ];
                writeln!(
                    out,
                    "{}",
                    values
                        .iter()
                        .map(|value| csv_escape(value))
                        .collect::<Vec<_>>()
                        .join(",")
                )?;
                rows += 1;
            }
        }
    }
    Ok(ExportReport {
        sqlite_path: options.sqlite_path.display().to_string(),
        out: options.out.display().to_string(),
        format: options.format.as_str().to_string(),
        rows,
    })
}

fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, DEFAULT_WORKERS_CAP)
}

fn default_walker_threads(_worker_count: usize) -> usize {
    env::var("QQ_ANALYZER_MEDIA_WALKERS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(4)
        .clamp(1, 32)
}

fn probe_job_queue_size(worker_count: usize) -> usize {
    env::var("QQ_ANALYZER_MEDIA_JOB_QUEUE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| worker_count.saturating_mul(8).max(32))
        .clamp(worker_count.max(1), 100_000)
}

fn probe_job_batch_size() -> usize {
    env::var("QQ_ANALYZER_MEDIA_JOB_BATCH")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PROBE_JOB_BATCH_SIZE)
        .clamp(1, 4096)
}

fn walk_event_queue_size() -> usize {
    env::var("QQ_ANALYZER_MEDIA_WALK_QUEUE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_WALK_EVENT_QUEUE_SIZE)
        .clamp(1024, 100_000)
}

fn progress_interval() -> Option<Duration> {
    env::var("QQ_ANALYZER_MEDIA_PROGRESS_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| (seconds > 0).then(|| Duration::from_secs(seconds)))
        .unwrap_or_else(|| Some(Duration::from_secs(DEFAULT_PROGRESS_INTERVAL_SECS)))
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(windows)]
fn dir_handle_cache_size() -> usize {
    env::var("QQ_ANALYZER_MEDIA_DIR_HANDLE_CACHE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(4096)
        .clamp(1, 65_536)
}

#[cfg(windows)]
impl WindowsProbeContext {
    fn from_env() -> Self {
        let io_ring_reader = if env_flag("QQ_ANALYZER_MEDIA_IORING") {
            match WindowsIoRingReader::new() {
                Ok(reader) => Some(reader),
                Err(err) => {
                    eprintln!("[media-info] IoRing disabled for worker: {err:#}");
                    None
                }
            }
        } else {
            None
        };
        let relative_open_cache = env_flag("QQ_ANALYZER_MEDIA_RELATIVE_OPEN")
            .then(|| WindowsRelativeOpenCache::new(dir_handle_cache_size()));
        Self {
            io_ring_reader,
            relative_open_cache,
        }
    }

    fn open_file(
        &mut self,
        path: &Path,
        overlapped: bool,
        timing: Option<&mut ProbeTiming>,
    ) -> anyhow::Result<File> {
        if let Some(cache) = self.relative_open_cache.as_mut() {
            cache.open_file(path, overlapped, timing)
        } else {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
            if overlapped {
                options.custom_flags(FILE_FLAG_OVERLAPPED);
            }
            options.open(path).map_err(Into::into)
        }
    }

    fn read_io_ring_probe_data(
        &mut self,
        path: &Path,
        read_len: usize,
        file_size: u64,
        mut timing: Option<&mut ProbeTiming>,
    ) -> anyhow::Result<(Vec<u8>, Option<TailProbe>)> {
        let open_started = timing.as_ref().map(|_| Instant::now());
        let file = self.open_file(path, true, timing.as_deref_mut())?;
        if let (Some(timing), Some(started)) = (timing.as_deref_mut(), open_started) {
            timing.open += started.elapsed();
        }
        if read_len == 0 {
            let close_started = timing.as_ref().map(|_| Instant::now());
            drop(file);
            if let (Some(timing), Some(started)) = (timing.as_deref_mut(), close_started) {
                timing.close += started.elapsed();
            }
            return Ok((Vec::new(), None));
        }

        let mut data = self.read_io_ring_window(
            file.as_raw_handle() as HANDLE,
            0,
            read_len,
            timing.as_deref_mut(),
        )?;
        let tail = if let Some(plan) = mp4_tail_read_plan(&data, file_size) {
            match plan {
                TailReadPlan::Append { offset, len } => {
                    let extra = self.read_io_ring_window(
                        file.as_raw_handle() as HANDLE,
                        offset,
                        len,
                        timing.as_deref_mut(),
                    )?;
                    data.extend_from_slice(&extra);
                    None
                }
                TailReadPlan::Tail { offset, len } => Some(TailProbe {
                    offset,
                    data: self.read_io_ring_window(
                        file.as_raw_handle() as HANDLE,
                        offset,
                        len,
                        timing.as_deref_mut(),
                    )?,
                }),
            }
        } else {
            None
        };
        let close_started = timing.as_ref().map(|_| Instant::now());
        drop(file);
        if let (Some(timing), Some(started)) = (timing.as_deref_mut(), close_started) {
            timing.close += started.elapsed();
        }
        Ok((data, tail))
    }

    fn read_io_ring_window(
        &mut self,
        handle: HANDLE,
        offset: u64,
        read_len: usize,
        mut timing: Option<&mut ProbeTiming>,
    ) -> anyhow::Result<Vec<u8>> {
        let read_started = timing.as_ref().map(|_| Instant::now());
        let data = self
            .io_ring_reader
            .as_mut()
            .context("IoRing reader is not initialized")?
            .read_handle_at(handle, offset, read_len)?;
        if let (Some(timing), Some(started)) = (timing.as_deref_mut(), read_started) {
            timing.read += started.elapsed();
            timing.bytes_read = timing.bytes_read.saturating_add(data.len() as u64);
            if data.len() < read_len {
                timing.short_reads = timing.short_reads.saturating_add(1);
            }
        }
        Ok(data)
    }
}

#[cfg(not(windows))]
impl WindowsProbeContext {
    fn from_env() -> Self {
        Self
    }
}

#[cfg(windows)]
impl WindowsRelativeOpenCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            dirs: VecDeque::new(),
        }
    }

    fn open_file(
        &mut self,
        path: &Path,
        overlapped: bool,
        mut timing: Option<&mut ProbeTiming>,
    ) -> anyhow::Result<File> {
        if let Some(timing) = timing.as_deref_mut() {
            timing.relative_open_files = timing.relative_open_files.saturating_add(1);
        }
        let parent = path
            .parent()
            .with_context(|| format!("{} has no parent directory", path.display()))?;
        let file_name = path
            .file_name()
            .with_context(|| format!("{} has no file name", path.display()))?;
        let dir_handle = self.dir_handle(parent, timing.as_deref_mut())?;
        nt_create_file_relative(dir_handle, file_name, overlapped)
            .with_context(|| format!("relative NtCreateFile {}", path.display()))
    }

    fn dir_handle(
        &mut self,
        dir: &Path,
        mut timing: Option<&mut ProbeTiming>,
    ) -> anyhow::Result<HANDLE> {
        if let Some(index) = self.dirs.iter().position(|cached| cached.path == dir) {
            if let Some(timing) = timing.as_deref_mut() {
                timing.dir_cache_hits = timing.dir_cache_hits.saturating_add(1);
            }
            let cached = self.dirs.remove(index).expect("cached dir index exists");
            let handle = cached.handle.as_raw_handle() as HANDLE;
            self.dirs.push_front(cached);
            return Ok(handle);
        }

        if let Some(timing) = timing.as_deref_mut() {
            timing.dir_cache_misses = timing.dir_cache_misses.saturating_add(1);
        }
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
            .with_context(|| format!("open parent directory {}", dir.display()))?;
        let raw = handle.as_raw_handle() as HANDLE;
        self.dirs.push_front(WindowsCachedDir {
            path: dir.to_path_buf(),
            handle,
        });
        while self.dirs.len() > self.capacity {
            self.dirs.pop_back();
        }
        Ok(raw)
    }
}

#[cfg(windows)]
fn nt_create_file_relative(
    root_directory: HANDLE,
    file_name: &std::ffi::OsStr,
    overlapped: bool,
) -> anyhow::Result<File> {
    let mut file_name_wide = file_name
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let name_bytes = (file_name_wide.len().saturating_sub(1) * 2) as u16;
    let unicode_name = UNICODE_STRING {
        Length: name_bytes,
        MaximumLength: (file_name_wide.len() * 2).min(usize::from(u16::MAX)) as u16,
        Buffer: file_name_wide.as_mut_ptr(),
    };
    let attributes = NtObjectAttributes {
        length: std::mem::size_of::<NtObjectAttributes>() as u32,
        root_directory,
        object_name: &unicode_name,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut io_status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let mut handle: HANDLE = INVALID_HANDLE_VALUE;
    let create_options = if overlapped {
        NT_FILE_NON_DIRECTORY_FILE
    } else {
        NT_FILE_NON_DIRECTORY_FILE | NT_FILE_SYNCHRONOUS_IO_NONALERT
    };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &attributes,
            &mut io_status,
            std::ptr::null_mut(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            NT_FILE_OPEN,
            create_options,
            std::ptr::null_mut(),
            0,
        )
    };
    check_ntstatus(status, "NtCreateFile")?;
    if handle == INVALID_HANDLE_VALUE {
        anyhow::bail!("NtCreateFile returned INVALID_HANDLE_VALUE");
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

#[cfg(windows)]
impl WindowsIoRingReader {
    fn new() -> anyhow::Result<Self> {
        unsafe {
            let mut caps: IORING_CAPABILITIES = std::mem::zeroed();
            check_hresult(
                QueryIoRingCapabilities(&mut caps),
                "QueryIoRingCapabilities",
            )?;
            let version = if caps.MaxVersion >= IORING_VERSION_3 {
                IORING_VERSION_3
            } else if caps.MaxVersion >= IORING_VERSION_2 {
                IORING_VERSION_2
            } else {
                IORING_VERSION_1
            };
            let flags = IORING_CREATE_FLAGS {
                Required: IORING_CREATE_REQUIRED_FLAGS_NONE,
                Advisory: IORING_CREATE_ADVISORY_FLAGS_NONE,
            };
            let mut ring = std::ptr::null_mut();
            check_hresult(
                CreateIoRing(version, flags, 256, 256, &mut ring),
                "CreateIoRing",
            )?;
            if IsIoRingOpSupported(ring, IORING_OP_READ) == 0 {
                let _ = CloseIoRing(ring);
                anyhow::bail!("IoRing read operation is not supported");
            }
            Ok(Self { ring })
        }
    }

    fn read_handle_at(
        &mut self,
        handle: HANDLE,
        offset: u64,
        read_len: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let mut data = Vec::<u8>::with_capacity(read_len);
        unsafe {
            let file_ref = IORING_HANDLE_REF {
                Kind: IORING_REF_RAW,
                Handle: IORING_HANDLE_REF_0 { Handle: handle },
            };
            let buffer_ref = IORING_BUFFER_REF {
                Kind: IORING_REF_RAW,
                Buffer: IORING_BUFFER_REF_0 {
                    Address: data.as_mut_ptr().cast(),
                },
            };
            let user_data = data.as_ptr() as usize;
            check_hresult(
                BuildIoRingReadFile(
                    self.ring,
                    file_ref,
                    buffer_ref,
                    read_len as u32,
                    offset,
                    user_data,
                    IOSQE_FLAGS_NONE,
                ),
                "BuildIoRingReadFile",
            )?;

            let mut submitted = 0u32;
            check_hresult(
                SubmitIoRing(self.ring, 1, IORING_WAIT_INFINITE_MS, &mut submitted),
                "SubmitIoRing",
            )?;
            if submitted == 0 {
                anyhow::bail!("SubmitIoRing submitted 0 entries");
            }

            let mut cqe: IORING_CQE = std::mem::zeroed();
            check_hresult(
                PopIoRingCompletion(self.ring, &mut cqe),
                "PopIoRingCompletion",
            )?;
            check_hresult(cqe.ResultCode, "IoRing read completion")?;
            let bytes_read = cqe.Information.min(read_len) as usize;
            data.set_len(bytes_read);
            Ok(data)
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsIoRingReader {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseIoRing(self.ring);
        }
    }
}

#[cfg(windows)]
fn check_hresult(hr: i32, operation: &str) -> anyhow::Result<()> {
    if hr < 0 {
        anyhow::bail!("{operation} failed with HRESULT 0x{:08x}", hr as u32);
    }
    Ok(())
}

#[cfg(windows)]
fn check_ntstatus(status: i32, operation: &str) -> anyhow::Result<()> {
    if status < 0 {
        anyhow::bail!("{operation} failed with NTSTATUS 0x{:08x}", status as u32);
    }
    Ok(())
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn truncate_progress_path(path: &str) -> String {
    const MAX_LEN: usize = 180;
    if path.chars().count() <= MAX_LEN {
        return path.to_string();
    }
    let suffix: String = path
        .chars()
        .rev()
        .take(MAX_LEN - 3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{suffix}")
}

fn build_serial(
    con: &mut Connection,
    options: &BuildOptions,
    roots: &[PathBuf],
    report: &mut BuildReport,
    progress: &mut ProgressReporter,
    profiler: &mut BuildProfiler,
) -> anyhow::Result<()> {
    let mut writer = ProbeWriteBatch::new();
    let mut windows_probe_context = WindowsProbeContext::from_env();
    for (root_index, root) in roots.iter().enumerate() {
        let started = Instant::now();
        let mut walker = MediaFileWalker::new(root);
        while report.scanned_files < options.max_files {
            let entry = profiler.time(ProfileStage::Enumerate, || walker.next());
            let Some(entry) = entry else {
                break;
            };
            progress.observe_path(&entry.path);
            report.files_seen += 1;
            report.root_stats[root_index].files_seen += 1;
            if profiler.time(ProfileStage::SkipQuery, || {
                should_skip_unchanged(con, &entry, options.force)
            })? {
                report.unchanged_files += 1;
                report.root_stats[root_index].unchanged_files += 1;
                progress.maybe_emit(report, writer.pending_len(), profiler);
                continue;
            }
            report.scanned_files += 1;
            report.root_stats[root_index].scanned_files += 1;
            let mut probe_timing = ProbeTiming::default();
            let probe_started = profiler.enabled().then(Instant::now);
            let probe = if profiler.enabled() {
                match probe_entry_worker(
                    &entry,
                    Some(root),
                    options.hash_mode,
                    Some(&mut probe_timing),
                    &mut windows_probe_context,
                ) {
                    Ok(probe) => probe,
                    Err(err) => error_probe(&entry, Some(root), options.hash_mode, err),
                }
            } else {
                match probe_entry_worker(
                    &entry,
                    Some(root),
                    options.hash_mode,
                    None,
                    &mut windows_probe_context,
                ) {
                    Ok(probe) => probe,
                    Err(err) => error_probe(&entry, Some(root), options.hash_mode, err),
                }
            };
            if let Some(started) = probe_started {
                profiler.add_duration(ProfileStage::Probe, started.elapsed());
                profiler.add_probe_timing(probe_timing);
            }
            writer.push(
                con,
                report,
                ProbeResult {
                    root_index,
                    probe,
                    probe_elapsed: Duration::ZERO,
                    worker_wait_elapsed: Duration::ZERO,
                    probe_timing: ProbeTiming::default(),
                },
                profiler,
            )?;
            progress.maybe_emit(report, writer.pending_len(), profiler);
        }
        finish_root_stats(report, root_index, &walker, started);
    }
    writer.flush(con, profiler)?;
    Ok(())
}

fn build_parallel(
    con: &mut Connection,
    options: &BuildOptions,
    roots: &[PathBuf],
    report: &mut BuildReport,
    progress: &mut ProgressReporter,
    profiler: &mut BuildProfiler,
) -> anyhow::Result<()> {
    let worker_count = options.workers;
    let (job_tx, job_rx) = bounded::<ProbeJob>(probe_job_queue_size(worker_count));
    let (result_tx, result_rx) = unbounded::<ProbeResult>();
    let profile_worker_probe = profiler.enabled();
    for _ in 0..worker_count {
        let job_rx = job_rx.clone();
        let result_tx = result_tx.clone();
        let hash_mode = options.hash_mode;
        std::thread::spawn(move || {
            let mut windows_probe_context = WindowsProbeContext::from_env();
            loop {
                let wait_started = profile_worker_probe.then(Instant::now);
                let job = match job_rx.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                };
                let worker_wait_elapsed = wait_started
                    .map(|started| started.elapsed())
                    .unwrap_or(Duration::ZERO);
                for (entry_index, entry) in job.entries.into_iter().enumerate() {
                    let probe_started = profile_worker_probe.then(Instant::now);
                    let mut probe_timing = ProbeTiming::default();
                    let probe = if profile_worker_probe {
                        match probe_entry_worker(
                            &entry,
                            Some(&job.source_root),
                            hash_mode,
                            Some(&mut probe_timing),
                            &mut windows_probe_context,
                        ) {
                            Ok(probe) => probe,
                            Err(err) => error_probe(&entry, Some(&job.source_root), hash_mode, err),
                        }
                    } else {
                        match probe_entry_worker(
                            &entry,
                            Some(&job.source_root),
                            hash_mode,
                            None,
                            &mut windows_probe_context,
                        ) {
                            Ok(probe) => probe,
                            Err(err) => error_probe(&entry, Some(&job.source_root), hash_mode, err),
                        }
                    };
                    let probe_elapsed = probe_started
                        .map(|started| started.elapsed())
                        .unwrap_or(Duration::ZERO);
                    let result_wait_elapsed = if entry_index == 0 {
                        worker_wait_elapsed
                    } else {
                        Duration::ZERO
                    };
                    if result_tx
                        .send(ProbeResult {
                            root_index: job.root_index,
                            probe,
                            probe_elapsed,
                            worker_wait_elapsed: result_wait_elapsed,
                            probe_timing,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    }
    drop(result_tx);

    let mut sent_jobs = 0usize;
    let mut received_jobs = 0usize;
    let mut writer = ProbeWriteBatch::new();
    if report.walkers <= 1 {
        for (root_index, root) in roots.iter().enumerate() {
            let started = Instant::now();
            let mut walker = MediaFileWalker::new(root);
            while report.scanned_files < options.max_files {
                drain_results(
                    con,
                    report,
                    &mut writer,
                    &result_rx,
                    &mut received_jobs,
                    progress,
                    profiler,
                    false,
                )?;
                let entry = profiler.time(ProfileStage::Enumerate, || walker.next());
                let Some(entry) = entry else {
                    break;
                };
                progress.observe_path(&entry.path);
                report.files_seen += 1;
                report.root_stats[root_index].files_seen += 1;
                if profiler.time(ProfileStage::SkipQuery, || {
                    should_skip_unchanged(con, &entry, options.force)
                })? {
                    report.unchanged_files += 1;
                    report.root_stats[root_index].unchanged_files += 1;
                    progress.maybe_emit(report, writer.pending_len(), profiler);
                    continue;
                }
                report.scanned_files += 1;
                report.root_stats[root_index].scanned_files += 1;
                profiler.time(ProfileStage::QueueSend, || {
                    job_tx.send(ProbeJob {
                        root_index,
                        source_root: root.clone(),
                        entries: vec![entry],
                    })
                })?;
                sent_jobs += 1;
                profiler.record_inflight(sent_jobs.saturating_sub(received_jobs));
                progress.maybe_emit(report, writer.pending_len(), profiler);
            }
            finish_root_stats(report, root_index, &walker, started);
        }
        drop(job_tx);
        while received_jobs < sent_jobs {
            drain_results(
                con,
                report,
                &mut writer,
                &result_rx,
                &mut received_jobs,
                progress,
                profiler,
                true,
            )?;
        }
        writer.flush(con, profiler)?;
        return Ok(());
    }

    let walk_started = Instant::now();
    let stop_walkers = Arc::new(AtomicBool::new(false));
    let (walk_rx, walker_handles) =
        spawn_parallel_walkers(roots, report.walkers, Arc::clone(&stop_walkers));
    while report.scanned_files < options.max_files {
        drain_results(
            con,
            report,
            &mut writer,
            &result_rx,
            &mut received_jobs,
            progress,
            profiler,
            false,
        )?;
        let event = match profiler.time(ProfileStage::WalkNext, || {
            walk_rx.recv_timeout(Duration::from_millis(10))
        }) {
            Ok(event) => event,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        match event {
            WalkEvent::Stats {
                root_index,
                directories_seen,
                errors,
                enumerate_elapsed,
            } => {
                profiler.add_duration(ProfileStage::Enumerate, enumerate_elapsed);
                report.root_stats[root_index].directories_seen += directories_seen;
                report.root_stats[root_index].walk_errors += errors;
                report.walk_errors += errors;
            }
            WalkEvent::Entries {
                root_index,
                source_root,
                entries,
            } => {
                let mut probe_entries = Vec::with_capacity(entries.len());
                for entry in entries {
                    if report.scanned_files >= options.max_files {
                        break;
                    }
                    progress.observe_path(&entry.path);
                    report.files_seen += 1;
                    report.root_stats[root_index].files_seen += 1;
                    if profiler.time(ProfileStage::SkipQuery, || {
                        should_skip_unchanged(con, &entry, options.force)
                    })? {
                        report.unchanged_files += 1;
                        report.root_stats[root_index].unchanged_files += 1;
                        continue;
                    }
                    report.scanned_files += 1;
                    report.root_stats[root_index].scanned_files += 1;
                    probe_entries.push(entry);
                }
                if !probe_entries.is_empty() {
                    let batch_len = probe_entries.len();
                    profiler.time(ProfileStage::QueueSend, || {
                        job_tx.send(ProbeJob {
                            root_index,
                            source_root,
                            entries: probe_entries,
                        })
                    })?;
                    sent_jobs += batch_len;
                    profiler.record_inflight(sent_jobs.saturating_sub(received_jobs));
                }
                progress.maybe_emit(report, writer.pending_len(), profiler);
            }
        }
    }
    stop_walkers.store(true, Ordering::Relaxed);
    drop(walk_rx);
    for handle in walker_handles {
        let _ = handle.join();
    }
    for root_stat in &mut report.root_stats {
        root_stat.elapsed_ms = walk_started.elapsed().as_millis() as u64;
    }
    drop(job_tx);
    while received_jobs < sent_jobs {
        drain_results(
            con,
            report,
            &mut writer,
            &result_rx,
            &mut received_jobs,
            progress,
            profiler,
            true,
        )?;
    }
    writer.flush(con, profiler)?;
    Ok(())
}

fn repair_serial(
    con: &mut Connection,
    options: &RepairOptions,
    targets: Vec<RepairTarget>,
    report: &mut BuildReport,
    counters: &mut RepairCounters,
    progress: &mut ProgressReporter,
    profiler: &mut BuildProfiler,
) -> anyhow::Result<()> {
    let mut writer = ProbeWriteBatch::new();
    let mut windows_probe_context = WindowsProbeContext::from_env();
    for target in targets {
        progress.observe_path(&target.entry.path);
        report.files_seen += 1;
        report.scanned_files += 1;
        report.root_stats[0].files_seen += 1;
        report.root_stats[0].scanned_files += 1;
        let mut probe_timing = ProbeTiming::default();
        let probe_started = profiler.enabled().then(Instant::now);
        let probe = if profiler.enabled() {
            match probe_entry_worker(
                &target.entry,
                target.source_root.as_deref(),
                options.hash_mode,
                Some(&mut probe_timing),
                &mut windows_probe_context,
            ) {
                Ok(probe) => probe,
                Err(err) => error_probe(
                    &target.entry,
                    target.source_root.as_deref(),
                    options.hash_mode,
                    err,
                ),
            }
        } else {
            match probe_entry_worker(
                &target.entry,
                target.source_root.as_deref(),
                options.hash_mode,
                None,
                &mut windows_probe_context,
            ) {
                Ok(probe) => probe,
                Err(err) => error_probe(
                    &target.entry,
                    target.source_root.as_deref(),
                    options.hash_mode,
                    err,
                ),
            }
        };
        if let Some(started) = probe_started {
            profiler.add_duration(ProfileStage::Probe, started.elapsed());
            profiler.add_probe_timing(probe_timing);
        }
        record_repair_counters(counters, &target.old, &probe);
        writer.push(
            con,
            report,
            ProbeResult {
                root_index: 0,
                probe,
                probe_elapsed: Duration::ZERO,
                worker_wait_elapsed: Duration::ZERO,
                probe_timing: ProbeTiming::default(),
            },
            profiler,
        )?;
        progress.maybe_emit(report, writer.pending_len(), profiler);
    }
    writer.flush(con, profiler)?;
    Ok(())
}

fn repair_parallel(
    con: &mut Connection,
    options: &RepairOptions,
    targets: Vec<RepairTarget>,
    report: &mut BuildReport,
    counters: &mut RepairCounters,
    progress: &mut ProgressReporter,
    profiler: &mut BuildProfiler,
) -> anyhow::Result<()> {
    let worker_count = options.workers;
    let (job_tx, job_rx) = bounded::<RepairJob>(probe_job_queue_size(worker_count));
    let (result_tx, result_rx) = unbounded::<RepairProbeResult>();
    let profile_worker_probe = profiler.enabled();
    for _ in 0..worker_count {
        let job_rx = job_rx.clone();
        let result_tx = result_tx.clone();
        let hash_mode = options.hash_mode;
        std::thread::spawn(move || {
            let mut windows_probe_context = WindowsProbeContext::from_env();
            loop {
                let wait_started = profile_worker_probe.then(Instant::now);
                let job = match job_rx.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                };
                let worker_wait_elapsed = wait_started
                    .map(|started| started.elapsed())
                    .unwrap_or(Duration::ZERO);
                for (target_index, target) in job.targets.into_iter().enumerate() {
                    let probe_started = profile_worker_probe.then(Instant::now);
                    let mut probe_timing = ProbeTiming::default();
                    let probe = if profile_worker_probe {
                        match probe_entry_worker(
                            &target.entry,
                            target.source_root.as_deref(),
                            hash_mode,
                            Some(&mut probe_timing),
                            &mut windows_probe_context,
                        ) {
                            Ok(probe) => probe,
                            Err(err) => error_probe(
                                &target.entry,
                                target.source_root.as_deref(),
                                hash_mode,
                                err,
                            ),
                        }
                    } else {
                        match probe_entry_worker(
                            &target.entry,
                            target.source_root.as_deref(),
                            hash_mode,
                            None,
                            &mut windows_probe_context,
                        ) {
                            Ok(probe) => probe,
                            Err(err) => error_probe(
                                &target.entry,
                                target.source_root.as_deref(),
                                hash_mode,
                                err,
                            ),
                        }
                    };
                    let probe_elapsed = probe_started
                        .map(|started| started.elapsed())
                        .unwrap_or(Duration::ZERO);
                    let result_wait_elapsed = if target_index == 0 {
                        worker_wait_elapsed
                    } else {
                        Duration::ZERO
                    };
                    if result_tx
                        .send(RepairProbeResult {
                            old: target.old,
                            result: ProbeResult {
                                root_index: 0,
                                probe,
                                probe_elapsed,
                                worker_wait_elapsed: result_wait_elapsed,
                                probe_timing,
                            },
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    }
    drop(result_tx);

    let mut sent_targets = 0usize;
    let mut received_targets = 0usize;
    let mut writer = ProbeWriteBatch::new();
    let batch_size = probe_job_batch_size();
    let mut pending = Vec::with_capacity(batch_size);
    for target in targets {
        progress.observe_path(&target.entry.path);
        report.files_seen += 1;
        report.scanned_files += 1;
        report.root_stats[0].files_seen += 1;
        report.root_stats[0].scanned_files += 1;
        pending.push(target);
        if pending.len() >= batch_size {
            let batch_len = pending.len();
            profiler.time(ProfileStage::QueueSend, || {
                job_tx.send(RepairJob {
                    targets: std::mem::take(&mut pending),
                })
            })?;
            sent_targets += batch_len;
            profiler.record_inflight(sent_targets.saturating_sub(received_targets));
            drain_repair_results(
                con,
                report,
                counters,
                &mut writer,
                &result_rx,
                &mut received_targets,
                progress,
                profiler,
                false,
            )?;
        }
        progress.maybe_emit(report, writer.pending_len(), profiler);
    }
    if !pending.is_empty() {
        let batch_len = pending.len();
        profiler.time(ProfileStage::QueueSend, || {
            job_tx.send(RepairJob { targets: pending })
        })?;
        sent_targets += batch_len;
        profiler.record_inflight(sent_targets.saturating_sub(received_targets));
    }
    drop(job_tx);
    while received_targets < sent_targets {
        drain_repair_results(
            con,
            report,
            counters,
            &mut writer,
            &result_rx,
            &mut received_targets,
            progress,
            profiler,
            true,
        )?;
    }
    writer.flush(con, profiler)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drain_repair_results(
    con: &mut Connection,
    report: &mut BuildReport,
    counters: &mut RepairCounters,
    writer: &mut ProbeWriteBatch,
    result_rx: &Receiver<RepairProbeResult>,
    received_targets: &mut usize,
    progress: &mut ProgressReporter,
    profiler: &mut BuildProfiler,
    block: bool,
) -> anyhow::Result<()> {
    loop {
        let repair_result = if block {
            match profiler.time(ProfileStage::ResultReceive, || result_rx.recv()) {
                Ok(result) => result,
                Err(_) => return Ok(()),
            }
        } else {
            match profiler.time(ProfileStage::ResultReceive, || result_rx.try_recv()) {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        };
        profiler.add_duration(
            ProfileStage::WorkerWait,
            repair_result.result.worker_wait_elapsed,
        );
        profiler.add_duration(ProfileStage::Probe, repair_result.result.probe_elapsed);
        profiler.add_probe_timing(repair_result.result.probe_timing);
        record_repair_counters(counters, &repair_result.old, &repair_result.result.probe);
        writer.push(con, report, repair_result.result, profiler)?;
        *received_targets += 1;
        progress.maybe_emit(report, writer.pending_len(), profiler);
        if block {
            return Ok(());
        }
    }
}

fn record_repair_counters(counters: &mut RepairCounters, old: &RepairOldClass, probe: &MediaProbe) {
    if old.media_kind != probe.file.media_kind
        || old.container != probe.file.container
        || old.detected_format != probe.file.detected_format
        || old.mime != probe.file.mime
        || old.scan_status != probe.file.scan_status
        || old.error != probe.file.error
    {
        counters.changed_files += 1;
    }
    if old.scan_status != "ok" && probe.file.scan_status == "ok" {
        counters.fixed_files += 1;
    }
    if matches!(probe.file.scan_status.as_str(), "error" | "partial") {
        counters.still_problematic_files += 1;
    }
}

fn load_repair_targets(
    con: &Connection,
    max_files: usize,
    missing_files: &mut usize,
) -> anyhow::Result<Vec<RepairTarget>> {
    let limit = max_files.min(i64::MAX as usize) as i64;
    let mut stmt = con.prepare(
        r#"
WITH repair_targets AS (
  SELECT f.id, f.path, f.source_root, f.media_kind, f.container, f.detected_format,
         f.mime, f.scan_status, f.error
  FROM media_files f
  WHERE f.scan_status IN ('error', 'partial', 'unsupported')

  UNION

  SELECT f.id, f.path, f.source_root, f.media_kind, f.container, f.detected_format,
         f.mime, f.scan_status, f.error
  FROM media_files f
  WHERE f.container='mp4' AND f.media_kind='unknown'

  UNION

  SELECT f.id, f.path, f.source_root, f.media_kind, f.container, f.detected_format,
         f.mime, f.scan_status, f.error
  FROM media_streams s
  JOIN media_files f ON f.id=s.file_id
  WHERE s.stream_type='metadata' AND s.codec IN ('mp4', 'mov')

  UNION

  SELECT f.id, f.path, f.source_root, f.media_kind, f.container, f.detected_format,
         f.mime, f.scan_status, f.error
  FROM media_streams s
  JOIN media_files f ON f.id=s.file_id
  WHERE s.codec='mp3' AND (
    coalesce(lower(f.extension), '') <> 'mp3'
    OR coalesce(f.mime, '') <> 'audio/mpeg'
    OR s.sample_rate IS NULL
    OR coalesce(s.profile, '') = 'reserved'
  )
)
SELECT path, source_root, media_kind, container, detected_format, mime, scan_status, error
FROM repair_targets
ORDER BY path
LIMIT ?1
"#,
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            RepairOldClass {
                media_kind: row.get(2)?,
                container: row.get(3)?,
                detected_format: row.get(4)?,
                mime: row.get(5)?,
                scan_status: row.get(6)?,
                error: row.get(7)?,
            },
        ))
    })?;
    let mut targets = Vec::new();
    for row in rows {
        let (path_text, source_root, old) = row?;
        let path = PathBuf::from(&path_text);
        let entry = match media_file_entry_from_path(&path) {
            Ok(entry) => entry,
            Err(_) => {
                *missing_files += 1;
                continue;
            }
        };
        targets.push(RepairTarget {
            entry,
            source_root: source_root.map(PathBuf::from),
            old,
        });
    }
    Ok(targets)
}

struct WalkDirBatch {
    dirs: Vec<PathBuf>,
    entries: Vec<MediaFileEntry>,
    errors: usize,
}

fn spawn_parallel_walkers(
    roots: &[PathBuf],
    walker_count: usize,
    stop: Arc<AtomicBool>,
) -> (Receiver<WalkEvent>, Vec<std::thread::JoinHandle<()>>) {
    let (dir_tx, dir_rx) = unbounded::<WalkDirJob>();
    let (event_tx, event_rx) = bounded::<WalkEvent>(walk_event_queue_size());
    let pending_dirs = Arc::new(AtomicUsize::new(roots.len()));
    for (root_index, root) in roots.iter().enumerate() {
        let _ = dir_tx.send(WalkDirJob {
            root_index,
            source_root: root.clone(),
            dir: root.clone(),
        });
    }
    let mut handles = Vec::new();
    for _ in 0..walker_count.max(1) {
        let dir_rx = dir_rx.clone();
        let dir_tx = dir_tx.clone();
        let event_tx = event_tx.clone();
        let pending_dirs = Arc::clone(&pending_dirs);
        let stop = Arc::clone(&stop);
        let job_batch_size = probe_job_batch_size();
        handles.push(std::thread::spawn(move || loop {
            let job = match dir_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(job) => job,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if pending_dirs.load(Ordering::Acquire) == 0 {
                        break;
                    }
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            };
            if stop.load(Ordering::Relaxed) {
                pending_dirs.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let enumerate_started = Instant::now();
            let batch = enumerate_media_dir(&job.dir);
            let enumerate_elapsed = enumerate_started.elapsed();
            if event_tx
                .send(WalkEvent::Stats {
                    root_index: job.root_index,
                    directories_seen: 1,
                    errors: batch.errors,
                    enumerate_elapsed,
                })
                .is_err()
            {
                stop.store(true, Ordering::Relaxed);
            }
            if !stop.load(Ordering::Relaxed) {
                for dir in batch.dirs {
                    pending_dirs.fetch_add(1, Ordering::AcqRel);
                    if dir_tx
                        .send(WalkDirJob {
                            root_index: job.root_index,
                            source_root: job.source_root.clone(),
                            dir,
                        })
                        .is_err()
                    {
                        pending_dirs.fetch_sub(1, Ordering::AcqRel);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
            if !stop.load(Ordering::Relaxed) {
                let mut entries = batch.entries;
                while !entries.is_empty() {
                    let rest = if entries.len() > job_batch_size {
                        entries.split_off(job_batch_size)
                    } else {
                        Vec::new()
                    };
                    if event_tx
                        .send(WalkEvent::Entries {
                            root_index: job.root_index,
                            source_root: job.source_root.clone(),
                            entries,
                        })
                        .is_err()
                    {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    entries = rest;
                }
            }
            pending_dirs.fetch_sub(1, Ordering::AcqRel);
        }));
    }
    drop(dir_tx);
    drop(event_tx);
    (event_rx, handles)
}

#[cfg(not(windows))]
fn enumerate_media_dir(dir: &Path) -> WalkDirBatch {
    let mut batch = WalkDirBatch {
        dirs: Vec::new(),
        entries: Vec::new(),
        errors: 0,
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            batch.errors += 1;
            return batch;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                batch.errors += 1;
                continue;
            }
        };
        let ty = match entry.file_type() {
            Ok(ty) => ty,
            Err(_) => {
                batch.errors += 1;
                continue;
            }
        };
        let path = entry.path();
        if ty.is_dir() {
            batch.dirs.push(path);
        } else if ty.is_file() && should_consider_path(&path) {
            match entry.metadata() {
                Ok(metadata) => batch.entries.push(MediaFileEntry {
                    path,
                    file_size: metadata.len() as i64,
                    mtime_unix: modified_unix(&metadata),
                }),
                Err(_) => batch.errors += 1,
            }
        }
    }
    batch
}

#[cfg(windows)]
fn enumerate_media_dir(dir: &Path) -> WalkDirBatch {
    let mut batch = WalkDirBatch {
        dirs: Vec::new(),
        entries: Vec::new(),
        errors: 0,
    };
    let Some(mut state) = open_win32_find_state(dir.to_path_buf()) else {
        batch.errors += 1;
        return batch;
    };
    loop {
        if let Some(name) = win32_find_file_name(&state.data) {
            let path = state.dir.join(name);
            if state.data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                batch.dirs.push(path);
            } else if should_consider_path(&path) {
                batch.entries.push(MediaFileEntry {
                    path,
                    file_size: win32_find_file_size(&state.data) as i64,
                    mtime_unix: win32_filetime_to_unix(&state.data.ftLastWriteTime),
                });
            }
        }
        if !advance_win32_find_state(&mut state) {
            break;
        }
    }
    batch
}

#[allow(clippy::too_many_arguments)]
fn drain_results(
    con: &mut Connection,
    report: &mut BuildReport,
    writer: &mut ProbeWriteBatch,
    result_rx: &Receiver<ProbeResult>,
    received_jobs: &mut usize,
    progress: &mut ProgressReporter,
    profiler: &mut BuildProfiler,
    block: bool,
) -> anyhow::Result<()> {
    loop {
        let result = if block {
            match profiler.time(ProfileStage::ResultReceive, || result_rx.recv()) {
                Ok(result) => result,
                Err(_) => return Ok(()),
            }
        } else {
            match profiler.time(ProfileStage::ResultReceive, || result_rx.try_recv()) {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        };
        profiler.add_duration(ProfileStage::WorkerWait, result.worker_wait_elapsed);
        profiler.add_duration(ProfileStage::Probe, result.probe_elapsed);
        profiler.add_probe_timing(result.probe_timing);
        writer.push(con, report, result, profiler)?;
        *received_jobs += 1;
        progress.maybe_emit(report, writer.pending_len(), profiler);
        if block {
            return Ok(());
        }
    }
}

struct ProbeWriteBatch {
    pending: Vec<ProbeResult>,
    batch_size: usize,
}

impl ProbeWriteBatch {
    fn new() -> Self {
        Self {
            pending: Vec::with_capacity(sqlite_write_batch_size().min(16_384)),
            batch_size: sqlite_write_batch_size(),
        }
    }

    fn push(
        &mut self,
        con: &mut Connection,
        report: &mut BuildReport,
        result: ProbeResult,
        profiler: &mut BuildProfiler,
    ) -> anyhow::Result<()> {
        record_probe_report(report, result.root_index, &result.probe);
        self.pending.push(result);
        if self.pending.len() >= self.batch_size {
            self.flush(con, profiler)?;
        }
        Ok(())
    }

    fn flush(&mut self, con: &mut Connection, profiler: &mut BuildProfiler) -> anyhow::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let rows = self.pending.len();
        let flush_started = profiler.enabled().then(Instant::now);
        let tx = con.transaction()?;
        write_probes_tx(&tx, &self.pending, profiler)?;
        let commit_started = profiler.enabled().then(Instant::now);
        tx.commit()?;
        if let Some(started) = commit_started {
            profiler.add_duration(ProfileStage::SqliteCommit, started.elapsed());
        }
        if let Some(started) = flush_started {
            profiler.add_duration(ProfileStage::SqliteFlush, started.elapsed());
        }
        profiler.record_sqlite_flush(rows);
        self.pending.clear();
        Ok(())
    }

    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

fn finish_root_stats(
    report: &mut BuildReport,
    root_index: usize,
    walker: &MediaFileWalker,
    started: Instant,
) {
    report.root_stats[root_index].directories_seen = walker.directories_seen;
    report.root_stats[root_index].walk_errors = walker.errors;
    report.root_stats[root_index].elapsed_ms = started.elapsed().as_millis() as u64;
    report.walk_errors += walker.errors;
}

fn record_probe_report(report: &mut BuildReport, root_index: usize, probe: &MediaProbe) {
    report.indexed_files += 1;
    report.root_stats[root_index].indexed_files += 1;
    *report
        .by_media_kind
        .entry(probe.file.media_kind.clone())
        .or_insert(0) += 1;
    *report
        .by_container
        .entry(probe.file.container.clone())
        .or_insert(0) += 1;
    match probe.file.scan_status.as_str() {
        "unsupported" => {
            report.unsupported_files += 1;
            report.root_stats[root_index].unsupported_files += 1;
        }
        "partial" => {
            report.partial_files += 1;
            report.root_stats[root_index].partial_files += 1;
        }
        "error" => {
            report.error_files += 1;
            report.root_stats[root_index].error_files += 1;
        }
        _ => {}
    }
}

fn scan_roots(options: &BuildOptions) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if options.asset_roots.is_empty() {
        let account_root = options.root.join(&options.account);
        for name in [
            "Image",
            "nt_qq",
            "Video",
            "FileRecv",
            "MyCollection",
            "CustomFace",
        ] {
            push_existing_dir(&mut roots, account_root.join(name));
        }
    } else {
        for root in &options.asset_roots {
            push_existing_dir(&mut roots, root.clone());
        }
    }
    roots
}

fn push_existing_dir(roots: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() && !roots.iter().any(|existing| existing == &path) {
        roots.push(path);
    }
}

#[derive(Clone, Debug)]
struct MediaFileEntry {
    path: PathBuf,
    file_size: i64,
    mtime_unix: i64,
}

struct MediaFileWalker {
    pending_dirs: Vec<PathBuf>,
    #[cfg(not(windows))]
    current: Option<fs::ReadDir>,
    #[cfg(windows)]
    current: Option<Win32FindState>,
    directories_seen: usize,
    errors: usize,
}

#[cfg(windows)]
struct Win32FindState {
    dir: PathBuf,
    handle: HANDLE,
    data: WIN32_FIND_DATAW,
}

#[cfg(windows)]
impl Drop for Win32FindState {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe {
                FindClose(self.handle);
            }
        }
    }
}

impl MediaFileWalker {
    fn new(root: &Path) -> Self {
        Self {
            pending_dirs: vec![root.to_path_buf()],
            current: None,
            directories_seen: 0,
            errors: 0,
        }
    }
}

#[cfg(not(windows))]
impl Iterator for MediaFileWalker {
    type Item = MediaFileEntry;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let dir = self.pending_dirs.pop()?;
                self.directories_seen += 1;
                self.current = match fs::read_dir(dir) {
                    Ok(entries) => Some(entries),
                    Err(_) => {
                        self.errors += 1;
                        None
                    }
                };
                continue;
            }
            let entries = self.current.as_mut().expect("current read_dir exists");
            match entries.next() {
                Some(Ok(entry)) => {
                    let Ok(ty) = entry.file_type() else {
                        self.errors += 1;
                        continue;
                    };
                    let path = entry.path();
                    if ty.is_dir() {
                        self.pending_dirs.push(path);
                    } else if ty.is_file() && should_consider_path(&path) {
                        match entry.metadata() {
                            Ok(metadata) => {
                                return Some(MediaFileEntry {
                                    path,
                                    file_size: metadata.len() as i64,
                                    mtime_unix: modified_unix(&metadata),
                                });
                            }
                            Err(_) => self.errors += 1,
                        }
                    }
                }
                Some(Err(_)) => self.errors += 1,
                None => self.current = None,
            }
        }
    }
}

#[cfg(windows)]
impl Iterator for MediaFileWalker {
    type Item = MediaFileEntry;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let dir = self.pending_dirs.pop()?;
                self.directories_seen += 1;
                match open_win32_find_state(dir) {
                    Some(state) => self.current = Some(state),
                    None => {
                        self.errors += 1;
                        continue;
                    }
                }
            }
            let Some(state) = self.current.as_mut() else {
                continue;
            };
            let name = win32_find_file_name(&state.data);
            let entry = name.and_then(|name| {
                let path = state.dir.join(name);
                if state.data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    self.pending_dirs.push(path);
                    None
                } else if should_consider_path(&path) {
                    Some(MediaFileEntry {
                        path,
                        file_size: win32_find_file_size(&state.data) as i64,
                        mtime_unix: win32_filetime_to_unix(&state.data.ftLastWriteTime),
                    })
                } else {
                    None
                }
            });
            if !advance_win32_find_state(state) {
                self.current = None;
            }
            if entry.is_some() {
                return entry;
            }
        }
    }
}

#[cfg(windows)]
fn open_win32_find_state(dir: PathBuf) -> Option<Win32FindState> {
    let pattern = dir.join("*");
    let pattern_wide = windows_path_wide(&pattern);
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstFileExW(
            pattern_wide.as_ptr(),
            FindExInfoBasic,
            &mut data as *mut _ as *mut _,
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(Win32FindState { dir, handle, data })
    }
}

#[cfg(windows)]
fn advance_win32_find_state(state: &mut Win32FindState) -> bool {
    unsafe { FindNextFileW(state.handle, &mut state.data) != 0 }
}

#[cfg(windows)]
fn windows_path_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn win32_find_file_name(data: &WIN32_FIND_DATAW) -> Option<OsString> {
    let len = data
        .cFileName
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(data.cFileName.len());
    if len == 0 {
        return None;
    }
    let name = OsString::from_wide(&data.cFileName[..len]);
    if name == "." || name == ".." {
        None
    } else {
        Some(name)
    }
}

#[cfg(windows)]
fn win32_find_file_size(data: &WIN32_FIND_DATAW) -> u64 {
    ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64
}

#[cfg(windows)]
fn win32_filetime_to_unix(time: &windows_sys::Win32::Foundation::FILETIME) -> i64 {
    const WINDOWS_TO_UNIX_SECONDS: u64 = 11_644_473_600;
    let ticks = ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
    let seconds = ticks / 10_000_000;
    seconds
        .checked_sub(WINDOWS_TO_UNIX_SECONDS)
        .map(|value| value as i64)
        .unwrap_or(0)
}

fn should_consider_path(path: &Path) -> bool {
    let Some(ext) = extension_lower(path) else {
        return true;
    };
    !matches!(
        ext.as_str(),
        "db" | "sqlite"
            | "sqlite3"
            | "journal"
            | "wal"
            | "shm"
            | "idx"
            | "ini"
            | "log"
            | "json"
            | "jsonl"
            | "html"
            | "css"
            | "js"
            | "tmp"
            | "lock"
            | "bak"
    )
}

fn configure_connection(con: &Connection) -> anyhow::Result<()> {
    con.pragma_update(None, "journal_mode", "WAL")?;
    con.pragma_update(None, "synchronous", "NORMAL")?;
    con.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn init_schema(con: &Connection) -> anyhow::Result<()> {
    con.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS media_files (
  id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL,
  source_root TEXT,
  relative_path TEXT,
  file_name TEXT,
  extension TEXT,
  file_size INTEGER,
  mtime_unix INTEGER,
  media_kind TEXT,
  container TEXT,
  detected_format TEXT,
  mime TEXT,
  hash_mode TEXT DEFAULT 'none',
  sha256_hex TEXT NULL,
  probe_version TEXT,
  scan_status TEXT,
  error TEXT,
  scanned_at_unix INTEGER
);
CREATE TABLE IF NOT EXISTS media_streams (
  file_id INTEGER,
  stream_index INTEGER,
  stream_type TEXT,
  codec TEXT,
  codec_tag TEXT,
  profile TEXT,
  level TEXT,
  width INTEGER,
  height INTEGER,
  coded_width INTEGER,
  coded_height INTEGER,
  duration_ms INTEGER,
  bitrate INTEGER,
  fps_num INTEGER,
  fps_den INTEGER,
  sample_rate INTEGER,
  channels INTEGER,
  bit_depth INTEGER,
  pixel_format TEXT,
  chroma_subsampling TEXT,
  color_primaries TEXT,
  transfer_characteristics TEXT,
  matrix_coefficients TEXT,
  rotation_degrees INTEGER,
  details_json TEXT,
  PRIMARY KEY (file_id, stream_index),
  FOREIGN KEY(file_id) REFERENCES media_files(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS media_codec_features (
  file_id INTEGER,
  stream_index INTEGER,
  standard TEXT,
  coding_mode TEXT,
  transform TEXT,
  prediction TEXT,
  entropy_coding TEXT,
  progressive TEXT,
  lossless INTEGER,
  near_lossless INTEGER,
  alpha INTEGER,
  animation INTEGER,
  icc_profile INTEGER,
  exif INTEGER,
  xmp INTEGER,
  orientation INTEGER,
  confidence TEXT,
  features_json TEXT,
  FOREIGN KEY(file_id) REFERENCES media_files(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_media_files_kind_format ON media_files(media_kind, detected_format);
CREATE INDEX IF NOT EXISTS idx_media_files_path ON media_files(path);
CREATE INDEX IF NOT EXISTS idx_media_files_status ON media_files(scan_status);
CREATE INDEX IF NOT EXISTS idx_media_files_container_kind ON media_files(container, media_kind);
CREATE INDEX IF NOT EXISTS idx_media_streams_file_id ON media_streams(file_id);
CREATE INDEX IF NOT EXISTS idx_media_streams_codec ON media_streams(codec);
CREATE INDEX IF NOT EXISTS idx_media_codec_features_file_id ON media_codec_features(file_id);
CREATE INDEX IF NOT EXISTS idx_media_codec_features_standard ON media_codec_features(standard, coding_mode);
"#,
    )?;
    Ok(())
}

fn should_skip_unchanged(
    con: &Connection,
    entry: &MediaFileEntry,
    force: bool,
) -> anyhow::Result<bool> {
    if force {
        return Ok(false);
    }
    let path_text = path_string(&entry.path);
    let found = con
        .query_row(
            "SELECT 1 FROM media_files WHERE path=?1 AND file_size=?2 AND mtime_unix=?3 LIMIT 1",
            params![path_text, entry.file_size, entry.mtime_unix],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    Ok(found)
}

fn write_probes_tx(
    tx: &Transaction<'_>,
    pending: &[ProbeResult],
    profiler: &mut BuildProfiler,
) -> anyhow::Result<()> {
    let mut upsert_file = tx.prepare(
        r#"
INSERT INTO media_files (
  path, source_root, relative_path, file_name, extension, file_size, mtime_unix,
  media_kind, container, detected_format, mime, hash_mode, sha256_hex,
  probe_version, scan_status, error, scanned_at_unix
) VALUES (
  ?1, ?2, ?3, ?4, ?5, ?6, ?7,
  ?8, ?9, ?10, ?11, ?12, ?13,
  ?14, ?15, ?16, ?17
)
ON CONFLICT(path) DO UPDATE SET
  source_root=excluded.source_root,
  relative_path=excluded.relative_path,
  file_name=excluded.file_name,
  extension=excluded.extension,
  file_size=excluded.file_size,
  mtime_unix=excluded.mtime_unix,
  media_kind=excluded.media_kind,
  container=excluded.container,
  detected_format=excluded.detected_format,
  mime=excluded.mime,
  hash_mode=excluded.hash_mode,
  sha256_hex=excluded.sha256_hex,
  probe_version=excluded.probe_version,
  scan_status=excluded.scan_status,
  error=excluded.error,
  scanned_at_unix=excluded.scanned_at_unix
RETURNING id
"#,
    )?;
    let mut delete_streams = tx.prepare("DELETE FROM media_streams WHERE file_id=?1")?;
    let mut delete_features = tx.prepare("DELETE FROM media_codec_features WHERE file_id=?1")?;
    let mut insert_stream = tx.prepare(
        r#"
INSERT INTO media_streams (
  file_id, stream_index, stream_type, codec, codec_tag, profile, level,
  width, height, coded_width, coded_height, duration_ms, bitrate, fps_num, fps_den,
  sample_rate, channels, bit_depth, pixel_format, chroma_subsampling,
  color_primaries, transfer_characteristics, matrix_coefficients, rotation_degrees,
  details_json
) VALUES (
  ?1, ?2, ?3, ?4, ?5, ?6, ?7,
  ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
  ?16, ?17, ?18, ?19, ?20,
  ?21, ?22, ?23, ?24,
  ?25
)"#,
    )?;
    let mut insert_feature = tx.prepare(
        r#"
INSERT INTO media_codec_features (
  file_id, stream_index, standard, coding_mode, transform, prediction,
  entropy_coding, progressive, lossless, near_lossless, alpha, animation,
  icc_profile, exif, xmp, orientation, confidence, features_json
) VALUES (
  ?1, ?2, ?3, ?4, ?5, ?6,
  ?7, ?8, ?9, ?10, ?11, ?12,
  ?13, ?14, ?15, ?16, ?17, ?18
)"#,
    )?;
    for result in pending {
        let probe = &result.probe;
        let file_id: i64 = profiler.time(ProfileStage::SqliteUpsert, || {
            upsert_file.query_row(
                params![
                    probe.file.path,
                    probe.file.source_root,
                    probe.file.relative_path,
                    probe.file.file_name,
                    probe.file.extension,
                    probe.file.file_size,
                    probe.file.mtime_unix,
                    probe.file.media_kind,
                    probe.file.container,
                    probe.file.detected_format,
                    probe.file.mime,
                    probe.file.hash_mode,
                    probe.file.sha256_hex,
                    probe.file.probe_version,
                    probe.file.scan_status,
                    probe.file.error,
                    probe.file.scanned_at_unix,
                ],
                |row| row.get(0),
            )
        })?;
        profiler.time(ProfileStage::SqliteDelete, || -> rusqlite::Result<()> {
            delete_streams.execute(params![file_id])?;
            delete_features.execute(params![file_id])?;
            Ok(())
        })?;
        for stream in &probe.streams {
            profiler.time(ProfileStage::SqliteStreamInsert, || {
                insert_stream.execute(params![
                    file_id,
                    stream.stream_index,
                    stream.stream_type,
                    stream.codec,
                    stream.codec_tag,
                    stream.profile,
                    stream.level,
                    stream.width,
                    stream.height,
                    stream.coded_width,
                    stream.coded_height,
                    stream.duration_ms,
                    stream.bitrate,
                    stream.fps_num,
                    stream.fps_den,
                    stream.sample_rate,
                    stream.channels,
                    stream.bit_depth,
                    stream.pixel_format,
                    stream.chroma_subsampling,
                    stream.color_primaries,
                    stream.transfer_characteristics,
                    stream.matrix_coefficients,
                    stream.rotation_degrees,
                    stream.details_json,
                ])
            })?;
        }
        for feature in &probe.codec_features {
            profiler.time(ProfileStage::SqliteFeatureInsert, || {
                insert_feature.execute(params![
                    file_id,
                    feature.stream_index,
                    feature.standard,
                    feature.coding_mode,
                    feature.transform,
                    feature.prediction,
                    feature.entropy_coding,
                    feature.progressive,
                    feature.lossless,
                    feature.near_lossless,
                    feature.alpha,
                    feature.animation,
                    feature.icc_profile,
                    feature.exif,
                    feature.xmp,
                    feature.orientation,
                    feature.confidence,
                    feature.features_json,
                ])
            })?;
        }
    }
    Ok(())
}

fn probe_entry(
    entry: &MediaFileEntry,
    source_root: Option<&Path>,
    hash_mode: HashMode,
) -> anyhow::Result<MediaProbe> {
    probe_entry_timed(entry, source_root, hash_mode, None)
}

fn probe_entry_worker(
    entry: &MediaFileEntry,
    source_root: Option<&Path>,
    hash_mode: HashMode,
    timing: Option<&mut ProbeTiming>,
    windows_probe_context: &mut WindowsProbeContext,
) -> anyhow::Result<MediaProbe> {
    #[cfg(windows)]
    {
        if windows_probe_context.io_ring_reader.is_some() {
            return probe_entry_timed_io_ring(
                entry,
                source_root,
                hash_mode,
                windows_probe_context,
                timing,
            );
        }
        return probe_entry_timed_windows_context(
            entry,
            source_root,
            hash_mode,
            windows_probe_context,
            timing,
        );
    }
    #[cfg(not(windows))]
    {
        let _ = windows_probe_context;
        probe_entry_timed(entry, source_root, hash_mode, timing)
    }
}

fn probe_entry_timed(
    entry: &MediaFileEntry,
    source_root: Option<&Path>,
    hash_mode: HashMode,
    mut timing: Option<&mut ProbeTiming>,
) -> anyhow::Result<MediaProbe> {
    let mut file_record = base_file_record(entry, source_root, hash_mode);
    let read_len = (entry.file_size.max(0) as u64).min(probe_read_bytes()) as usize;
    let open_started = timing.as_ref().map(|_| Instant::now());
    let mut file = File::open(&entry.path)?;
    if let (Some(timing), Some(started)) = (timing.as_deref_mut(), open_started) {
        timing.open += started.elapsed();
    }
    let mut data = read_file_window(&mut file, 0, read_len, timing.as_deref_mut())?;
    let tail = read_mp4_tail_if_needed(
        &mut file,
        &mut data,
        entry.file_size.max(0) as u64,
        timing.as_deref_mut(),
    )?;
    let close_started = timing.as_ref().map(|_| Instant::now());
    drop(file);
    if let (Some(timing), Some(started)) = (timing.as_deref_mut(), close_started) {
        timing.close += started.elapsed();
    }
    if hash_mode == HashMode::Full {
        let hash_started = timing.as_ref().map(|_| Instant::now());
        file_record.sha256_hex = Some(sha256_file(&entry.path)?);
        if let (Some(timing), Some(started)) = (timing.as_deref_mut(), hash_started) {
            timing.hash += started.elapsed();
        }
    }
    if data.is_empty() {
        file_record.scan_status = "unsupported".to_string();
        file_record.error = Some("empty file".to_string());
        return Ok(MediaProbe {
            file: file_record,
            streams: Vec::new(),
            codec_features: Vec::new(),
        });
    }

    let parse_started = timing.as_ref().map(|_| Instant::now());
    let probe = parse_media_with_tail(
        &data,
        tail.as_ref(),
        entry.file_size.max(0) as u64,
        file_record,
    );
    if let (Some(timing), Some(started)) = (timing, parse_started) {
        timing.parse += started.elapsed();
    }
    Ok(probe)
}

#[cfg(windows)]
fn probe_entry_timed_windows_context(
    entry: &MediaFileEntry,
    source_root: Option<&Path>,
    hash_mode: HashMode,
    windows_probe_context: &mut WindowsProbeContext,
    mut timing: Option<&mut ProbeTiming>,
) -> anyhow::Result<MediaProbe> {
    let mut file_record = base_file_record(entry, source_root, hash_mode);
    let read_len = (entry.file_size.max(0) as u64).min(probe_read_bytes()) as usize;
    let open_started = timing.as_ref().map(|_| Instant::now());
    let mut file = windows_probe_context.open_file(&entry.path, false, timing.as_deref_mut())?;
    if let (Some(timing), Some(started)) = (timing.as_deref_mut(), open_started) {
        timing.open += started.elapsed();
    }
    let mut data = read_file_window(&mut file, 0, read_len, timing.as_deref_mut())?;
    let tail = read_mp4_tail_if_needed(
        &mut file,
        &mut data,
        entry.file_size.max(0) as u64,
        timing.as_deref_mut(),
    )?;
    let close_started = timing.as_ref().map(|_| Instant::now());
    drop(file);
    if let (Some(timing), Some(started)) = (timing.as_deref_mut(), close_started) {
        timing.close += started.elapsed();
    }
    if hash_mode == HashMode::Full {
        let hash_started = timing.as_ref().map(|_| Instant::now());
        file_record.sha256_hex = Some(sha256_file(&entry.path)?);
        if let (Some(timing), Some(started)) = (timing.as_deref_mut(), hash_started) {
            timing.hash += started.elapsed();
        }
    }
    if data.is_empty() {
        file_record.scan_status = "unsupported".to_string();
        file_record.error = Some("empty file".to_string());
        return Ok(MediaProbe {
            file: file_record,
            streams: Vec::new(),
            codec_features: Vec::new(),
        });
    }

    let parse_started = timing.as_ref().map(|_| Instant::now());
    let probe = parse_media_with_tail(
        &data,
        tail.as_ref(),
        entry.file_size.max(0) as u64,
        file_record,
    );
    if let (Some(timing), Some(started)) = (timing.as_deref_mut(), parse_started) {
        timing.parse += started.elapsed();
    }
    Ok(probe)
}

#[cfg(windows)]
fn probe_entry_timed_io_ring(
    entry: &MediaFileEntry,
    source_root: Option<&Path>,
    hash_mode: HashMode,
    windows_probe_context: &mut WindowsProbeContext,
    mut timing: Option<&mut ProbeTiming>,
) -> anyhow::Result<MediaProbe> {
    let mut file_record = base_file_record(entry, source_root, hash_mode);
    let read_len = (entry.file_size.max(0) as u64).min(probe_read_bytes()) as usize;
    let (data, tail) = windows_probe_context.read_io_ring_probe_data(
        &entry.path,
        read_len,
        entry.file_size.max(0) as u64,
        timing.as_deref_mut(),
    )?;
    if hash_mode == HashMode::Full {
        let hash_started = timing.as_ref().map(|_| Instant::now());
        file_record.sha256_hex = Some(sha256_file(&entry.path)?);
        if let (Some(timing), Some(started)) = (timing.as_deref_mut(), hash_started) {
            timing.hash += started.elapsed();
        }
    }
    if data.is_empty() {
        file_record.scan_status = "unsupported".to_string();
        file_record.error = Some("empty file".to_string());
        return Ok(MediaProbe {
            file: file_record,
            streams: Vec::new(),
            codec_features: Vec::new(),
        });
    }

    let parse_started = timing.as_ref().map(|_| Instant::now());
    let probe = parse_media_with_tail(
        &data,
        tail.as_ref(),
        entry.file_size.max(0) as u64,
        file_record,
    );
    if let (Some(timing), Some(started)) = (timing.as_deref_mut(), parse_started) {
        timing.parse += started.elapsed();
    }
    Ok(probe)
}

fn media_file_entry_from_path(path: &Path) -> anyhow::Result<MediaFileEntry> {
    let metadata = fs::metadata(path)?;
    Ok(MediaFileEntry {
        path: path.to_path_buf(),
        file_size: metadata.len() as i64,
        mtime_unix: modified_unix(&metadata),
    })
}

enum TailReadPlan {
    Append { offset: u64, len: usize },
    Tail { offset: u64, len: usize },
}

fn read_file_window(
    file: &mut File,
    offset: u64,
    read_len: usize,
    timing: Option<&mut ProbeTiming>,
) -> anyhow::Result<Vec<u8>> {
    if read_len == 0 {
        return Ok(Vec::new());
    }
    let read_started = timing.as_ref().map(|_| Instant::now());
    if offset != 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut data = Vec::with_capacity(read_len.min(1024 * 1024));
    std::io::Read::by_ref(file)
        .take(read_len as u64)
        .read_to_end(&mut data)?;
    if let (Some(timing), Some(started)) = (timing, read_started) {
        timing.read += started.elapsed();
        timing.bytes_read = timing.bytes_read.saturating_add(data.len() as u64);
        if data.len() < read_len {
            timing.short_reads = timing.short_reads.saturating_add(1);
        }
    }
    Ok(data)
}

fn read_mp4_tail_if_needed(
    file: &mut File,
    head: &mut Vec<u8>,
    file_size: u64,
    timing: Option<&mut ProbeTiming>,
) -> anyhow::Result<Option<TailProbe>> {
    let Some(plan) = mp4_tail_read_plan(head, file_size) else {
        return Ok(None);
    };
    match plan {
        TailReadPlan::Append { offset, len } => {
            let extra = read_file_window(file, offset, len, timing)?;
            head.extend_from_slice(&extra);
            Ok(None)
        }
        TailReadPlan::Tail { offset, len } => Ok(Some(TailProbe {
            offset,
            data: read_file_window(file, offset, len, timing)?,
        })),
    }
}

fn mp4_tail_read_plan(head: &[u8], file_size: u64) -> Option<TailReadPlan> {
    if !should_read_iso_bmff_tail(head, file_size) {
        return None;
    }
    let head_len = head.len() as u64;
    if file_size <= head_len {
        return None;
    }
    let remaining_budget = MAX_PROBE_READ_BYTES.saturating_sub(head_len);
    if remaining_budget == 0 {
        return None;
    }
    let tail_len = mp4_tail_read_bytes()
        .min(file_size)
        .min(remaining_budget)
        .min(usize::MAX as u64);
    if tail_len == 0 {
        return None;
    }
    let tail_offset = file_size.saturating_sub(tail_len);
    if tail_offset <= head_len {
        let extra_len = file_size
            .saturating_sub(head_len)
            .min(remaining_budget)
            .min(usize::MAX as u64) as usize;
        (extra_len > 0).then_some(TailReadPlan::Append {
            offset: head_len,
            len: extra_len,
        })
    } else {
        Some(TailReadPlan::Tail {
            offset: tail_offset,
            len: tail_len as usize,
        })
    }
}

fn should_read_iso_bmff_tail(head: &[u8], file_size: u64) -> bool {
    file_size > head.len() as u64 && is_iso_bmff(head) && !has_complete_top_level_box(head, b"moov")
}

fn probe_read_bytes() -> u64 {
    env::var("QQ_ANALYZER_MEDIA_PROBE_READ_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PROBE_READ_BYTES)
        .min(MAX_PROBE_READ_BYTES)
}

fn mp4_tail_read_bytes() -> u64 {
    env::var("QQ_ANALYZER_MEDIA_MP4_TAIL_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MP4_TAIL_READ_BYTES)
        .min(MAX_PROBE_READ_BYTES)
}

fn sqlite_write_batch_size() -> usize {
    env::var("QQ_ANALYZER_MEDIA_SQLITE_BATCH_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SQLITE_WRITE_BATCH_SIZE)
        .clamp(1, 100_000)
}

fn error_probe(
    entry: &MediaFileEntry,
    source_root: Option<&Path>,
    hash_mode: HashMode,
    err: anyhow::Error,
) -> MediaProbe {
    let mut file = base_file_record(entry, source_root, hash_mode);
    file.scan_status = "error".to_string();
    file.error = Some(err.to_string());
    MediaProbe {
        file,
        streams: Vec::new(),
        codec_features: Vec::new(),
    }
}

fn base_file_record(
    entry: &MediaFileEntry,
    source_root: Option<&Path>,
    hash_mode: HashMode,
) -> MediaFileRecord {
    let path = &entry.path;
    MediaFileRecord {
        path: path_string(path),
        source_root: source_root.map(path_string),
        relative_path: source_root
            .and_then(|root| path.strip_prefix(root).ok())
            .map(path_string),
        file_name: path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        extension: extension_lower(path),
        file_size: entry.file_size,
        mtime_unix: entry.mtime_unix,
        media_kind: "unknown".to_string(),
        container: "unknown".to_string(),
        detected_format: extension_lower(path).unwrap_or_else(|| "unknown".to_string()),
        mime: None,
        hash_mode: hash_mode.as_str().to_string(),
        sha256_hex: None,
        probe_version: PROBE_VERSION.to_string(),
        scan_status: "ok".to_string(),
        error: None,
        scanned_at_unix: now_unix(),
    }
}

fn parse_media_with_tail(
    data: &[u8],
    tail: Option<&TailProbe>,
    file_size: u64,
    mut file: MediaFileRecord,
) -> MediaProbe {
    if let Some(kind) = infer::get(data) {
        file.mime = Some(kind.mime_type().to_string());
        file.detected_format = kind.extension().to_string();
    }
    let parsed = if is_zero_filled_probe(data) {
        ParsedMedia::zero_filled_placeholder(&file, file_size > data.len() as u64)
    } else if is_jpeg(data) {
        parse_jpeg_with_file_size(data, file_size)
    } else if is_png(data) {
        parse_png_with_file_size(data, file_size)
    } else if is_gif(data) {
        parse_gif_with_file_size(data, file_size)
    } else if is_webp(data) {
        parse_webp_with_file_size(data, file_size)
    } else if is_jp2(data) || is_j2k(data) {
        parse_jpeg2000(data)
    } else if is_jxl(data) {
        parse_jpeg_xl(data)
    } else if is_iso_bmff(data) {
        parse_iso_bmff_with_tail(data, tail, file_size)
    } else if is_riff(data) {
        parse_riff(data, file_size)
    } else if is_mp3(data) {
        parse_mp3(data, file_size)
    } else if is_flac(data) {
        parse_flac(data)
    } else if is_ogg(data) {
        parse_ogg(data)
    } else if is_matroska(data) {
        parse_matroska(data)
    } else if is_silk(data) {
        parse_silk(data, file_size)
    } else if is_pdf(data) {
        ParsedMedia::document("pdf", "application/pdf")
    } else if is_svg(data) {
        parse_svg(data)
    } else if is_xml_document(data, &file) {
        ParsedMedia::document("xml", "text/xml")
    } else {
        ParsedMedia::unsupported(&file)
    };
    file.media_kind = parsed.media_kind;
    file.container = parsed.container;
    file.detected_format = parsed.detected_format;
    if file.mime.is_none() {
        file.mime = parsed.mime;
    }
    file.scan_status = parsed.scan_status;
    file.error = parsed.error;
    MediaProbe {
        file,
        streams: parsed.streams,
        codec_features: parsed.features,
    }
}

struct ParsedMedia {
    media_kind: String,
    container: String,
    detected_format: String,
    mime: Option<String>,
    scan_status: String,
    error: Option<String>,
    streams: Vec<MediaStreamRecord>,
    features: Vec<MediaCodecFeatureRecord>,
}

impl ParsedMedia {
    fn unsupported(file: &MediaFileRecord) -> Self {
        Self {
            media_kind: "unknown".to_string(),
            container: "unknown".to_string(),
            detected_format: file
                .mime
                .as_deref()
                .and_then(format_from_mime)
                .or_else(|| file.extension.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            mime: file.mime.clone(),
            scan_status: "unsupported".to_string(),
            error: Some("unsupported or unknown media format".to_string()),
            streams: Vec::new(),
            features: Vec::new(),
        }
    }

    fn document(format: &str, mime: &str) -> Self {
        Self {
            media_kind: "document".to_string(),
            container: "unknown".to_string(),
            detected_format: format.to_string(),
            mime: Some(mime.to_string()),
            scan_status: "ok".to_string(),
            error: None,
            streams: Vec::new(),
            features: Vec::new(),
        }
    }

    fn zero_filled_placeholder(file: &MediaFileRecord, prefix_only: bool) -> Self {
        Self {
            media_kind: "unknown".to_string(),
            container: "unknown".to_string(),
            detected_format: file
                .extension
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            mime: None,
            scan_status: "unsupported".to_string(),
            error: Some(if prefix_only {
                "zero-filled placeholder probe prefix".to_string()
            } else {
                "zero-filled placeholder".to_string()
            }),
            streams: Vec::new(),
            features: Vec::new(),
        }
    }
}

#[cfg(test)]
fn parse_jpeg(data: &[u8]) -> ParsedMedia {
    parse_jpeg_with_file_size(data, data.len() as u64)
}

fn parse_jpeg_with_file_size(data: &[u8], file_size: u64) -> ParsedMedia {
    let mut width = None;
    let mut height = None;
    let mut bit_depth = None;
    let mut components = None;
    let mut sof_marker = None;
    let mut restart_interval = None;
    let mut has_exif = false;
    let mut has_icc = false;
    let mut has_xmp = false;
    let mut has_jfif = false;
    let mut has_dht = false;
    let mut has_dac = false;
    let mut subsampling = None;
    let mut partial = false;
    let mut pos = 2usize;
    while pos + 1 < data.len() {
        if data[pos] != 0xff {
            pos += 1;
            continue;
        }
        while pos < data.len() && data[pos] == 0xff {
            pos += 1;
        }
        if pos >= data.len() {
            break;
        }
        let marker = data[pos];
        pos += 1;
        if marker == 0xd9 {
            break;
        }
        if marker == 0xda {
            break;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let Some(seg_len) = u16be(data, pos).map(|value| value as usize) else {
            partial = true;
            break;
        };
        if seg_len < 2 || pos + seg_len > data.len() {
            partial = true;
            break;
        }
        let seg = &data[pos + 2..pos + seg_len];
        match marker {
            0xc0..=0xcf | 0xf7 if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                sof_marker = Some(marker);
                if seg.len() >= 6 {
                    bit_depth = Some(seg[0] as i64);
                    height = Some(BigEndian::read_u16(&seg[1..3]) as i64);
                    width = Some(BigEndian::read_u16(&seg[3..5]) as i64);
                    components = Some(seg[5] as i64);
                    subsampling = jpeg_subsampling(seg);
                }
            }
            0xc4 => has_dht = true,
            0xcc => has_dac = true,
            0xdd => {
                if seg.len() >= 2 {
                    restart_interval = Some(BigEndian::read_u16(&seg[0..2]) as i64);
                }
            }
            0xe0 => {
                if seg.starts_with(b"JFIF\0") {
                    has_jfif = true;
                }
            }
            0xe1 => {
                if seg.starts_with(b"Exif\0\0") {
                    has_exif = true;
                }
                if seg.starts_with(b"http://ns.adobe.com/xap/1.0/\0") {
                    has_xmp = true;
                }
            }
            0xe2 => {
                if seg.starts_with(b"ICC_PROFILE\0") {
                    has_icc = true;
                }
            }
            _ => {}
        }
        pos += seg_len;
    }
    let (coding_mode, transform, progressive, lossless, entropy, standard) =
        jpeg_marker_features(sof_marker, has_dac, has_dht);
    let probe_truncated =
        partial && file_size > data.len() as u64 && width.is_some() && height.is_some();
    let details = json!({
        "sof_marker": sof_marker.map(|m| format!("0x{m:02x}")),
        "components": components,
        "restart_interval": restart_interval,
        "jfif": has_jfif,
        "app1_exif": has_exif,
        "app1_xmp": has_xmp,
        "app2_icc": has_icc,
        "dht": has_dht,
        "dac": has_dac,
        "probe_truncated": probe_truncated,
        "probe_read_bytes": data.len(),
        "file_size": file_size,
    });
    let (details_json, details_partial) = json_limited(&details);
    let stream = MediaStreamRecord {
        stream_index: 0,
        stream_type: "image".to_string(),
        codec: Some(standard.clone()),
        codec_tag: sof_marker.map(|m| format!("SOF{m:02x}")),
        width,
        height,
        bit_depth,
        pixel_format: components.map(|c| match c {
            1 => "grayscale".to_string(),
            3 => "ycbcr_or_rgb".to_string(),
            4 => "cmyk_or_ycck".to_string(),
            _ => format!("{c}_components"),
        }),
        chroma_subsampling: subsampling.clone(),
        details_json: Some(details_json),
        ..MediaStreamRecord::default()
    };
    let feature = MediaCodecFeatureRecord {
        stream_index: 0,
        standard: Some(standard.clone()),
        coding_mode: Some(coding_mode.to_string()),
        transform: Some(transform.to_string()),
        entropy_coding: Some(entropy.to_string()),
        progressive: Some(progressive.to_string()),
        lossless: Some(lossless as i64),
        icc_profile: Some(has_icc as i64),
        exif: Some(has_exif as i64),
        xmp: Some(has_xmp as i64),
        confidence: Some("header".to_string()),
        features_json: Some(
            serde_json::to_string(&json!({
                "subsampling": subsampling,
                "components": components,
                "precision": bit_depth,
            }))
            .unwrap_or_else(|_| "{}".to_string()),
        ),
        ..MediaCodecFeatureRecord::default()
    };
    ParsedMedia {
        media_kind: "image".to_string(),
        container: "jpeg".to_string(),
        detected_format: standard,
        mime: Some("image/jpeg".to_string()),
        scan_status: if (partial && !probe_truncated) || details_partial {
            "partial".to_string()
        } else {
            "ok".to_string()
        },
        error: (partial && !probe_truncated).then(|| "truncated JPEG marker segment".to_string()),
        streams: vec![stream],
        features: vec![feature],
    }
}

fn jpeg_marker_features(
    marker: Option<u8>,
    has_dac: bool,
    has_dht: bool,
) -> (&'static str, &'static str, bool, bool, &'static str, String) {
    match marker {
        Some(0xc2) | Some(0xc6) | Some(0xca) | Some(0xce) => (
            "progressive",
            "dct",
            true,
            false,
            if has_dac { "arithmetic" } else { "huffman" },
            "jpeg".to_string(),
        ),
        Some(0xc3) | Some(0xc7) | Some(0xcb) | Some(0xcf) => (
            "lossless",
            "lossless",
            false,
            true,
            if has_dac { "arithmetic" } else { "huffman" },
            "jpeg".to_string(),
        ),
        Some(0xf7) => (
            "jpeg-ls",
            "lossless_or_near_lossless",
            false,
            true,
            "golomb",
            "jpeg-ls".to_string(),
        ),
        _ => (
            "baseline_or_sequential",
            "dct",
            false,
            false,
            if has_dht || !has_dac {
                "huffman"
            } else {
                "arithmetic"
            },
            "jpeg".to_string(),
        ),
    }
}

fn jpeg_subsampling(sof: &[u8]) -> Option<String> {
    if sof.len() < 6 || sof[5] == 0 {
        return None;
    }
    let comps = sof[5] as usize;
    if sof.len() < 6 + comps * 3 {
        return None;
    }
    if comps == 1 {
        return Some("4:0:0".to_string());
    }
    let sampling = sof[7];
    let h = sampling >> 4;
    let v = sampling & 0x0f;
    match (h, v) {
        (1, 1) => Some("4:4:4".to_string()),
        (2, 1) => Some("4:2:2".to_string()),
        (2, 2) => Some("4:2:0".to_string()),
        (4, 1) => Some("4:1:1".to_string()),
        _ => Some(format!("{h}x{v}")),
    }
}

#[cfg(test)]
fn parse_png(data: &[u8]) -> ParsedMedia {
    parse_png_with_file_size(data, data.len() as u64)
}

fn parse_png_with_file_size(data: &[u8], file_size: u64) -> ParsedMedia {
    let mut width = None;
    let mut height = None;
    let mut bit_depth = None;
    let mut color_type = None;
    let mut interlace = None;
    let mut palette = false;
    let mut transparency = false;
    let mut gamma = false;
    let mut icc = false;
    let mut srgb = false;
    let mut exif = false;
    let mut xmp = false;
    let mut animation = false;
    let mut frame_count = None;
    let mut partial = false;
    let mut truncated_by_probe = false;
    let mut pos = 8usize;
    while pos + 12 <= data.len() {
        let len = BigEndian::read_u32(&data[pos..pos + 4]) as usize;
        let chunk_type = &data[pos + 4..pos + 8];
        if pos + 12 + len > data.len() {
            partial = true;
            truncated_by_probe = true;
            break;
        }
        let chunk = &data[pos + 8..pos + 8 + len];
        match chunk_type {
            b"IHDR" if chunk.len() >= 13 => {
                width = Some(BigEndian::read_u32(&chunk[0..4]) as i64);
                height = Some(BigEndian::read_u32(&chunk[4..8]) as i64);
                bit_depth = Some(chunk[8] as i64);
                color_type = Some(chunk[9]);
                interlace = Some(chunk[12]);
            }
            b"PLTE" => palette = true,
            b"tRNS" => transparency = true,
            b"gAMA" => gamma = true,
            b"iCCP" => icc = true,
            b"sRGB" => srgb = true,
            b"eXIf" => exif = true,
            b"iTXt" | b"tEXt" | b"zTXt" => {
                if chunk
                    .windows(29)
                    .any(|w| w == b"http://ns.adobe.com/xap/1.0/")
                {
                    xmp = true;
                }
            }
            b"acTL" if chunk.len() >= 8 => {
                animation = true;
                frame_count = Some(BigEndian::read_u32(&chunk[0..4]) as i64);
            }
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + len;
    }
    if pos < data.len() && pos + 12 > data.len() {
        partial = true;
        truncated_by_probe = true;
    }
    if width.is_none() || height.is_none() {
        partial = true;
    }
    let probe_truncated = partial
        && truncated_by_probe
        && file_size > data.len() as u64
        && width.is_some()
        && height.is_some();
    let alpha = matches!(color_type, Some(4 | 6)) || transparency;
    let details = json!({
        "color_type": color_type,
        "interlace": interlace,
        "palette": palette,
        "gamma": gamma,
        "srgb": srgb,
        "frame_count": frame_count,
        "probe_truncated": probe_truncated,
        "probe_read_bytes": data.len(),
        "file_size": file_size,
    });
    let (details_json, details_partial) = json_limited(&details);
    ParsedMedia {
        media_kind: if animation {
            "animated_image".to_string()
        } else {
            "image".to_string()
        },
        container: "png".to_string(),
        detected_format: "png".to_string(),
        mime: Some("image/png".to_string()),
        scan_status: if (partial && !probe_truncated) || details_partial {
            "partial".to_string()
        } else {
            "ok".to_string()
        },
        error: (partial && !probe_truncated).then(|| "truncated PNG chunk".to_string()),
        streams: vec![MediaStreamRecord {
            stream_index: 0,
            stream_type: "image".to_string(),
            codec: Some("png".to_string()),
            width,
            height,
            bit_depth,
            pixel_format: color_type.map(png_color_type),
            details_json: Some(details_json),
            ..MediaStreamRecord::default()
        }],
        features: vec![MediaCodecFeatureRecord {
            stream_index: 0,
            standard: Some("png".to_string()),
            coding_mode: Some("deflate".to_string()),
            lossless: Some(1),
            alpha: Some(alpha as i64),
            animation: Some(animation as i64),
            icc_profile: Some(icc as i64),
            exif: Some(exif as i64),
            xmp: Some(xmp as i64),
            confidence: Some("exact".to_string()),
            features_json: Some(
                serde_json::to_string(&json!({
                    "palette": palette,
                    "gamma": gamma,
                    "srgb": srgb,
                    "interlace": interlace,
                    "frame_count": frame_count,
                }))
                .unwrap_or_else(|_| "{}".to_string()),
            ),
            ..MediaCodecFeatureRecord::default()
        }],
    }
}

fn png_color_type(value: u8) -> String {
    match value {
        0 => "grayscale".to_string(),
        2 => "rgb".to_string(),
        3 => "indexed".to_string(),
        4 => "grayscale_alpha".to_string(),
        6 => "rgba".to_string(),
        other => format!("unknown_{other}"),
    }
}

#[cfg(test)]
fn parse_gif(data: &[u8]) -> ParsedMedia {
    parse_gif_with_file_size(data, data.len() as u64)
}

fn parse_gif_with_file_size(data: &[u8], file_size: u64) -> ParsedMedia {
    let width = u16le(data, 6).map(|v| v as i64);
    let height = u16le(data, 8).map(|v| v as i64);
    let packed = data.get(10).copied().unwrap_or(0);
    let global_palette = packed & 0x80 != 0;
    let global_palette_entries = if global_palette {
        Some(1i64 << ((packed & 0x07) + 1))
    } else {
        None
    };
    let mut pos = 13usize;
    if global_palette {
        pos = pos.saturating_add(3 * (global_palette_entries.unwrap_or(0) as usize));
    }
    let mut frames = 0i64;
    let mut transparent = false;
    let mut loop_count = None;
    let mut partial = false;
    let mut truncated_by_probe = false;
    while pos < data.len() {
        match data[pos] {
            0x2c => {
                if pos + 10 > data.len() {
                    partial = true;
                    truncated_by_probe = true;
                    break;
                }
                frames += 1;
                let local_packed = data[pos + 9];
                pos += 10;
                if local_packed & 0x80 != 0 {
                    pos = pos.saturating_add(3 * (1usize << ((local_packed & 0x07) + 1)));
                }
                if pos >= data.len() {
                    partial = true;
                    truncated_by_probe = true;
                    break;
                }
                pos += 1;
                match skip_gif_subblocks(data, pos) {
                    Some(next) => pos = next,
                    None => {
                        partial = true;
                        truncated_by_probe = true;
                        break;
                    }
                }
            }
            0x21 => {
                if pos + 2 > data.len() {
                    partial = true;
                    truncated_by_probe = true;
                    break;
                }
                let label = data[pos + 1];
                pos += 2;
                if label == 0xf9 && pos + 5 <= data.len() && data[pos] == 4 {
                    transparent |= data[pos + 1] & 0x01 != 0;
                    pos += 6;
                } else if label == 0xff {
                    if pos >= data.len() {
                        partial = true;
                        truncated_by_probe = true;
                        break;
                    }
                    let block_size = data[pos] as usize;
                    if pos + 1 + block_size > data.len() {
                        partial = true;
                        truncated_by_probe = true;
                        break;
                    }
                    let app = &data[pos + 1..pos + 1 + block_size];
                    pos += 1 + block_size;
                    if app.starts_with(b"NETSCAPE")
                        && pos + 5 <= data.len()
                        && data[pos] == 3
                        && data[pos + 1] == 1
                    {
                        loop_count = Some(u16le(data, pos + 2).unwrap_or(0) as i64);
                    }
                    match skip_gif_subblocks(data, pos) {
                        Some(next) => pos = next,
                        None => {
                            partial = true;
                            truncated_by_probe = true;
                            break;
                        }
                    }
                } else {
                    match skip_gif_subblocks(data, pos) {
                        Some(next) => pos = next,
                        None => {
                            partial = true;
                            truncated_by_probe = true;
                            break;
                        }
                    }
                }
            }
            0x3b => break,
            _ => {
                partial = true;
                break;
            }
        }
    }
    let animation = frames > 1;
    let probe_truncated = partial
        && truncated_by_probe
        && file_size > data.len() as u64
        && width.is_some()
        && height.is_some();
    let details = json!({
        "version": String::from_utf8_lossy(&data[3..6.min(data.len())]).to_string(),
        "global_palette": global_palette,
        "global_palette_entries": global_palette_entries,
        "frames": frames,
        "loop_count": loop_count,
        "transparent": transparent,
        "probe_truncated": probe_truncated,
        "probe_read_bytes": data.len(),
        "file_size": file_size,
    });
    let (details_json, details_partial) = json_limited(&details);
    ParsedMedia {
        media_kind: if animation {
            "animated_image".to_string()
        } else {
            "image".to_string()
        },
        container: "gif".to_string(),
        detected_format: "gif".to_string(),
        mime: Some("image/gif".to_string()),
        scan_status: if (partial && !probe_truncated) || details_partial {
            "partial".to_string()
        } else {
            "ok".to_string()
        },
        error: (partial && !probe_truncated).then(|| "truncated GIF block".to_string()),
        streams: vec![MediaStreamRecord {
            stream_index: 0,
            stream_type: "image".to_string(),
            codec: Some("gif".to_string()),
            width,
            height,
            bit_depth: Some(((packed & 0x07) + 1) as i64),
            details_json: Some(details_json),
            ..MediaStreamRecord::default()
        }],
        features: vec![MediaCodecFeatureRecord {
            stream_index: 0,
            standard: Some("gif".to_string()),
            coding_mode: Some("lzw".to_string()),
            lossless: Some(1),
            alpha: Some(transparent as i64),
            animation: Some(animation as i64),
            confidence: Some("exact".to_string()),
            features_json: Some(
                serde_json::to_string(&json!({
                    "frames": frames,
                    "loop_count": loop_count,
                    "global_palette": global_palette,
                }))
                .unwrap_or_else(|_| "{}".to_string()),
            ),
            ..MediaCodecFeatureRecord::default()
        }],
    }
}

fn skip_gif_subblocks(data: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *data.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            return Some(pos);
        }
        pos = pos.checked_add(len)?;
        if pos > data.len() {
            return None;
        }
    }
}

#[cfg(test)]
fn parse_webp(data: &[u8]) -> ParsedMedia {
    parse_webp_with_file_size(data, data.len() as u64)
}

fn parse_webp_with_file_size(data: &[u8], file_size: u64) -> ParsedMedia {
    let mut width = None;
    let mut height = None;
    let mut codec = "webp".to_string();
    let mut lossless = false;
    let mut alpha = false;
    let mut animation = false;
    let mut chunks = Vec::new();
    let mut partial = false;
    let mut truncated_by_probe = false;
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let typ = fourcc(&data[pos..pos + 4]);
        let len = LittleEndian::read_u32(&data[pos + 4..pos + 8]) as usize;
        if pos + 8 + len > data.len() {
            partial = true;
            truncated_by_probe = true;
            break;
        }
        let chunk = &data[pos + 8..pos + 8 + len];
        chunks.push(typ.clone());
        match typ.as_str() {
            "VP8X" if chunk.len() >= 10 => {
                alpha |= chunk[0] & 0x10 != 0;
                animation |= chunk[0] & 0x02 != 0;
                width = Some((read_u24le(&chunk[4..7]) + 1) as i64);
                height = Some((read_u24le(&chunk[7..10]) + 1) as i64);
                codec = "webp-vp8x".to_string();
            }
            "VP8L" if chunk.len() >= 5 && chunk[0] == 0x2f => {
                let b1 = chunk[1] as u32;
                let b2 = chunk[2] as u32;
                let b3 = chunk[3] as u32;
                let b4 = chunk[4] as u32;
                width = Some((1 + (((b2 & 0x3f) << 8) | b1)) as i64);
                height = Some((1 + (((b4 & 0x0f) << 10) | (b3 << 2) | ((b2 & 0xc0) >> 6))) as i64);
                alpha |= b4 & 0x10 != 0;
                lossless = true;
                codec = "webp-vp8l".to_string();
            }
            "VP8 " if chunk.len() >= 10 && chunk[3..6] == [0x9d, 0x01, 0x2a] => {
                width = Some((LittleEndian::read_u16(&chunk[6..8]) & 0x3fff) as i64);
                height = Some((LittleEndian::read_u16(&chunk[8..10]) & 0x3fff) as i64);
                codec = "webp-vp8".to_string();
            }
            "ALPH" => alpha = true,
            "ANIM" | "ANMF" => animation = true,
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    if pos < data.len() && pos + 8 > data.len() {
        partial = true;
        truncated_by_probe = true;
    }
    let probe_truncated = partial
        && truncated_by_probe
        && file_size > data.len() as u64
        && width.is_some()
        && height.is_some();
    let details = json!({
        "chunks": chunks,
        "probe_truncated": probe_truncated,
        "probe_read_bytes": data.len(),
        "file_size": file_size,
    });
    let (details_json, details_partial) = json_limited(&details);
    ParsedMedia {
        media_kind: if animation {
            "animated_image".to_string()
        } else {
            "image".to_string()
        },
        container: "webp".to_string(),
        detected_format: "webp".to_string(),
        mime: Some("image/webp".to_string()),
        scan_status: if (partial && !probe_truncated) || details_partial {
            "partial".to_string()
        } else {
            "ok".to_string()
        },
        error: (partial && !probe_truncated).then(|| "truncated WebP chunk".to_string()),
        streams: vec![MediaStreamRecord {
            stream_index: 0,
            stream_type: "image".to_string(),
            codec: Some(codec.clone()),
            width,
            height,
            details_json: Some(details_json),
            ..MediaStreamRecord::default()
        }],
        features: vec![MediaCodecFeatureRecord {
            stream_index: 0,
            standard: Some("webp".to_string()),
            coding_mode: Some(if lossless { "vp8l" } else { "vp8" }.to_string()),
            lossless: Some(lossless as i64),
            alpha: Some(alpha as i64),
            animation: Some(animation as i64),
            confidence: Some("exact".to_string()),
            features_json: Some(
                serde_json::to_string(&json!({
                    "codec": codec,
                    "lossless": lossless,
                    "alpha": alpha,
                    "animation": animation,
                }))
                .unwrap_or_else(|_| "{}".to_string()),
            ),
            ..MediaCodecFeatureRecord::default()
        }],
    }
}

fn parse_jpeg2000(data: &[u8]) -> ParsedMedia {
    let mut width = None;
    let mut height = None;
    let mut components = None;
    let mut bit_depth = None;
    let mut progression_order = None;
    let mut transform = None;
    let details = if is_jp2(data) {
        let mut pos = 0usize;
        while let Some(header) = read_box(data, pos, data.len()) {
            if header.typ == *b"jp2h" {
                let mut child = header.data_start;
                while let Some(inner) = read_box(data, child, header.end) {
                    if inner.typ == *b"ihdr" {
                        let chunk = &data[inner.data_start..inner.end];
                        if chunk.len() >= 14 {
                            height = Some(BigEndian::read_u32(&chunk[0..4]) as i64);
                            width = Some(BigEndian::read_u32(&chunk[4..8]) as i64);
                            components = Some(BigEndian::read_u16(&chunk[8..10]) as i64);
                            let bpc = chunk[10];
                            if bpc != 0xff {
                                bit_depth = Some(((bpc & 0x7f) + 1) as i64);
                            }
                        }
                    }
                    child = inner.end;
                }
            }
            if header.typ == *b"jp2c" {
                let codestream = &data[header.data_start..header.end];
                parse_j2k_codestream(
                    codestream,
                    &mut width,
                    &mut height,
                    &mut components,
                    &mut bit_depth,
                    &mut progression_order,
                    &mut transform,
                );
            }
            pos = header.end;
        }
        json!({ "container": "jp2" })
    } else {
        parse_j2k_codestream(
            data,
            &mut width,
            &mut height,
            &mut components,
            &mut bit_depth,
            &mut progression_order,
            &mut transform,
        );
        json!({ "container": "codestream" })
    };
    let (details_json, details_partial) = json_limited(&details);
    ParsedMedia {
        media_kind: "image".to_string(),
        container: "jpeg2000".to_string(),
        detected_format: if is_jp2(data) { "jp2" } else { "j2k" }.to_string(),
        mime: Some(if is_jp2(data) {
            "image/jp2".to_string()
        } else {
            "image/j2k".to_string()
        }),
        scan_status: if details_partial { "partial" } else { "ok" }.to_string(),
        error: None,
        streams: vec![MediaStreamRecord {
            stream_index: 0,
            stream_type: "image".to_string(),
            codec: Some("jpeg2000".to_string()),
            width,
            height,
            bit_depth,
            details_json: Some(details_json),
            ..MediaStreamRecord::default()
        }],
        features: vec![MediaCodecFeatureRecord {
            stream_index: 0,
            standard: Some("jpeg2000".to_string()),
            coding_mode: Some("wavelet".to_string()),
            transform,
            lossless: None,
            confidence: Some("header".to_string()),
            features_json: Some(
                serde_json::to_string(&json!({
                    "components": components,
                    "progression_order": progression_order,
                }))
                .unwrap_or_else(|_| "{}".to_string()),
            ),
            ..MediaCodecFeatureRecord::default()
        }],
    }
}

fn parse_j2k_codestream(
    data: &[u8],
    width: &mut Option<i64>,
    height: &mut Option<i64>,
    components: &mut Option<i64>,
    bit_depth: &mut Option<i64>,
    progression_order: &mut Option<String>,
    transform: &mut Option<String>,
) {
    let mut pos = if data.starts_with(&[0xff, 0x4f]) {
        2
    } else {
        0
    };
    while pos + 4 <= data.len() {
        if data[pos] != 0xff {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        pos += 2;
        if matches!(marker, 0x90 | 0xd9) {
            break;
        }
        let Some(len) = u16be(data, pos).map(|value| value as usize) else {
            break;
        };
        if len < 2 || pos + len > data.len() {
            break;
        }
        let seg = &data[pos + 2..pos + len];
        match marker {
            0x51 if seg.len() >= 36 => {
                let xsiz = BigEndian::read_u32(&seg[2..6]);
                let ysiz = BigEndian::read_u32(&seg[6..10]);
                let xosiz = BigEndian::read_u32(&seg[10..14]);
                let yosiz = BigEndian::read_u32(&seg[14..18]);
                *width = Some(xsiz.saturating_sub(xosiz) as i64);
                *height = Some(ysiz.saturating_sub(yosiz) as i64);
                *components = Some(BigEndian::read_u16(&seg[34..36]) as i64);
                if seg.len() >= 37 {
                    *bit_depth = Some(((seg[36] & 0x7f) + 1) as i64);
                }
            }
            0x52 if seg.len() >= 10 => {
                *progression_order = Some(
                    match seg[1] {
                        0 => "LRCP",
                        1 => "RLCP",
                        2 => "RPCL",
                        3 => "PCRL",
                        4 => "CPRL",
                        _ => "unknown",
                    }
                    .to_string(),
                );
                *transform = Some(
                    match seg[9] {
                        0 => "9/7 irreversible",
                        1 => "5/3 reversible",
                        _ => "unknown",
                    }
                    .to_string(),
                );
            }
            _ => {}
        }
        pos += len;
    }
}

fn parse_jpeg_xl(data: &[u8]) -> ParsedMedia {
    let container = data.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n");
    let details = json!({
        "container": container,
        "codestream_magic": data.starts_with(&[0xff, 0x0a]),
    });
    let (details_json, details_partial) = json_limited(&details);
    ParsedMedia {
        media_kind: "image".to_string(),
        container: "jpegxl".to_string(),
        detected_format: "jxl".to_string(),
        mime: Some("image/jxl".to_string()),
        scan_status: if details_partial { "partial" } else { "ok" }.to_string(),
        error: None,
        streams: vec![MediaStreamRecord {
            stream_index: 0,
            stream_type: "image".to_string(),
            codec: Some("jpegxl".to_string()),
            details_json: Some(details_json),
            ..MediaStreamRecord::default()
        }],
        features: vec![MediaCodecFeatureRecord {
            stream_index: 0,
            standard: Some("jpegxl".to_string()),
            confidence: Some("header".to_string()),
            features_json: Some(
                serde_json::to_string(&json!({ "container": container }))
                    .unwrap_or_else(|_| "{}".to_string()),
            ),
            ..MediaCodecFeatureRecord::default()
        }],
    }
}

#[cfg(test)]
fn parse_iso_bmff(data: &[u8], file_size: u64) -> ParsedMedia {
    parse_iso_bmff_with_tail(data, None, file_size)
}

fn parse_iso_bmff_with_tail(data: &[u8], tail: Option<&TailProbe>, file_size: u64) -> ParsedMedia {
    let mut major_brand = None;
    let mut minor_version = None;
    let mut brands = Vec::new();
    let mut movie_timescale = None;
    let mut movie_duration = None;
    let mut tracks = Vec::new();
    let mut partial = false;
    let mut moov_source = None::<String>;
    let mut tail_moov_offset = None::<u64>;
    let mut pos = 0usize;
    while let Some(header) = read_box(data, pos, data.len()) {
        match &header.typ {
            b"ftyp" => {
                let payload = &data[header.data_start..header.end];
                if payload.len() >= 8 {
                    major_brand = Some(fourcc(&payload[0..4]));
                    minor_version = Some(BigEndian::read_u32(&payload[4..8]));
                    let mut off = 8usize;
                    while off + 4 <= payload.len() {
                        brands.push(fourcc(&payload[off..off + 4]));
                        off += 4;
                    }
                }
            }
            b"moov" => {
                let movie = parse_moov(data, header.data_start, header.end);
                movie_timescale = movie.timescale;
                movie_duration = movie.duration_ms;
                tracks = movie.tracks;
                moov_source = Some("head".to_string());
            }
            _ => {}
        }
        pos = header.end;
        if header.end >= data.len() {
            break;
        }
    }
    if tracks.is_empty() {
        if let Some((movie, offset)) = tail.and_then(parse_moov_from_tail) {
            movie_timescale = movie.timescale.or(movie_timescale);
            movie_duration = movie.duration_ms.or(movie_duration);
            tracks = movie.tracks;
            moov_source = Some("tail".to_string());
            tail_moov_offset = Some(offset);
        }
    }
    if file_size > data.len() as u64 && tracks.is_empty() {
        partial = true;
    }
    let format = iso_format(major_brand.as_deref(), &brands);
    let media_kind = if format == "avif" || format == "heif" || format == "heic" {
        if brands
            .iter()
            .any(|brand| brand == "avis" || brand == "msf1")
        {
            "animated_image"
        } else {
            "image"
        }
    } else if tracks.iter().any(|track| track.stream_type == "video") {
        "video"
    } else if tracks.iter().any(|track| track.stream_type == "audio") {
        "audio"
    } else {
        "unknown"
    };
    let details = json!({
        "major_brand": major_brand,
        "minor_version": minor_version,
        "compatible_brands": brands,
        "movie_timescale": movie_timescale,
        "movie_duration_ms": movie_duration,
        "moov_source": moov_source,
        "tail_probe_offset": tail.map(|tail| tail.offset),
        "tail_probe_bytes": tail.map(|tail| tail.data.len()),
        "tail_moov_offset": tail_moov_offset,
    });
    let (details_json, details_partial) = json_limited(&details);
    let container_details = details.clone();
    let mut streams = Vec::new();
    let mut features = Vec::new();
    for (idx, track) in tracks.into_iter().enumerate() {
        let mut stream = track.stream;
        stream.stream_index = idx as i64;
        stream.details_json = Some(match stream.details_json.take() {
            Some(track_details_json) => {
                let track_details = serde_json::from_str::<Value>(&track_details_json)
                    .unwrap_or_else(|_| {
                        json!({
                            "raw_track_details_json": track_details_json,
                        })
                    });
                json_limited(&json!({
                    "container": container_details.clone(),
                    "track": track_details,
                }))
                .0
            }
            None => details_json.clone(),
        });
        if let Some(feature) = track.feature {
            let mut feature = feature;
            feature.stream_index = idx as i64;
            features.push(feature);
        }
        streams.push(stream);
    }
    if streams.is_empty() {
        streams.push(MediaStreamRecord {
            stream_index: 0,
            stream_type: if media_kind == "image" || media_kind == "animated_image" {
                "image".to_string()
            } else {
                "metadata".to_string()
            },
            codec: Some(format.clone()),
            duration_ms: movie_duration,
            details_json: Some(details_json),
            ..MediaStreamRecord::default()
        });
    }
    ParsedMedia {
        media_kind: media_kind.to_string(),
        container: "mp4".to_string(),
        detected_format: format.clone(),
        mime: Some(
            match format.as_str() {
                "avif" => "image/avif",
                "heif" | "heic" => "image/heif",
                "m4a" => "audio/mp4",
                "3gp" => "video/3gpp",
                "mov" => "video/quicktime",
                _ => "video/mp4",
            }
            .to_string(),
        ),
        scan_status: if partial || details_partial {
            "partial".to_string()
        } else if media_kind == "unknown" {
            "unsupported".to_string()
        } else {
            "ok".to_string()
        },
        error: if partial {
            Some("moov/tracks not found in probe head/tail windows".to_string())
        } else if media_kind == "unknown" {
            Some("no supported media tracks found".to_string())
        } else {
            None
        },
        streams,
        features,
    }
}

fn parse_moov_from_tail(tail: &TailProbe) -> Option<(MovieMeta, u64)> {
    let data = &tail.data;
    let mut fallback = None;
    for pos in 0..data.len().saturating_sub(8) {
        if &data[pos + 4..pos + 8] != b"moov" {
            continue;
        }
        let Some(header) = read_box(data, pos, data.len()) else {
            continue;
        };
        if header.typ != *b"moov" {
            continue;
        }
        let movie = parse_moov(data, header.data_start, header.end);
        let absolute_offset = tail.offset.saturating_add(pos as u64);
        if !movie.tracks.is_empty() {
            return Some((movie, absolute_offset));
        }
        fallback.get_or_insert((movie, absolute_offset));
    }
    fallback
}

#[derive(Default)]
struct MovieMeta {
    timescale: Option<i64>,
    duration_ms: Option<i64>,
    tracks: Vec<IsoTrackOutput>,
}

struct IsoTrackOutput {
    stream_type: String,
    stream: MediaStreamRecord,
    feature: Option<MediaCodecFeatureRecord>,
}

#[derive(Default)]
struct IsoTrackMeta {
    handler: Option<String>,
    timescale: Option<u64>,
    duration: Option<u64>,
    width: Option<i64>,
    height: Option<i64>,
    rotation_degrees: Option<i64>,
    sample_count: Option<i64>,
    sample_delta: Option<u64>,
    sample_entries: Vec<IsoSampleEntry>,
}

struct IsoSampleEntry {
    tag: String,
    codec: String,
    width: Option<i64>,
    height: Option<i64>,
    channels: Option<i64>,
    sample_rate: Option<i64>,
    bit_depth: Option<i64>,
    bitrate: Option<i64>,
    pixel_format: Option<String>,
    chroma_subsampling: Option<String>,
    color_primaries: Option<String>,
    transfer_characteristics: Option<String>,
    matrix_coefficients: Option<String>,
    profile: Option<String>,
    level: Option<String>,
    details: Value,
}

fn parse_moov(data: &[u8], start: usize, end: usize) -> MovieMeta {
    let mut movie = MovieMeta::default();
    let mut pos = start;
    while let Some(header) = read_box(data, pos, end) {
        match &header.typ {
            b"mvhd" => {
                let (timescale, duration_ms) = parse_mvhd(&data[header.data_start..header.end]);
                movie.timescale = timescale.map(|v| v as i64);
                movie.duration_ms = duration_ms.map(|v| v as i64);
            }
            b"trak" => {
                if let Some(track) = parse_trak(data, header.data_start, header.end) {
                    movie.tracks.push(track);
                }
            }
            _ => {}
        }
        pos = header.end;
    }
    movie
}

fn parse_trak(data: &[u8], start: usize, end: usize) -> Option<IsoTrackOutput> {
    let mut meta = IsoTrackMeta::default();
    let mut pos = start;
    while let Some(header) = read_box(data, pos, end) {
        match &header.typ {
            b"tkhd" => parse_tkhd(&data[header.data_start..header.end], &mut meta),
            b"mdia" => parse_mdia(data, header.data_start, header.end, &mut meta),
            _ => {}
        }
        pos = header.end;
    }
    let stream_type = match meta.handler.as_deref() {
        Some("vide") | Some("pict") | Some("auxv") => "video",
        Some("soun") => "audio",
        Some("subt") | Some("text") | Some("sbtl") => "subtitle",
        _ => "metadata",
    }
    .to_string();
    let entry = meta.sample_entries.first();
    let duration_ms = match (meta.duration, meta.timescale) {
        (Some(duration), Some(timescale)) if timescale > 0 => {
            Some(((duration as u128 * 1000) / timescale as u128) as i64)
        }
        _ => None,
    };
    let details = json!({
        "handler": meta.handler,
        "sample_entries": meta.sample_entries.iter().map(|entry| &entry.details).collect::<Vec<_>>(),
        "track_timescale": meta.timescale,
        "track_duration": meta.duration,
        "sample_count": meta.sample_count,
        "sample_delta": meta.sample_delta,
    });
    let (details_json, _) = json_limited(&details);
    let codec = entry.map(|entry| entry.codec.clone());
    let codec_tag = entry.map(|entry| entry.tag.clone());
    let (fps_num, fps_den) = if stream_type == "video" {
        match (meta.timescale, meta.sample_delta) {
            (Some(timescale), Some(delta)) if timescale > 0 && delta > 0 => {
                Some((timescale as i64, delta as i64))
            }
            _ => None,
        }
    } else {
        None
    }
    .unwrap_or((0, 0));
    let stream = MediaStreamRecord {
        stream_index: 0,
        stream_type: stream_type.clone(),
        codec: codec.clone(),
        codec_tag,
        profile: entry.and_then(|entry| entry.profile.clone()),
        level: entry.and_then(|entry| entry.level.clone()),
        width: meta.width.or_else(|| entry.and_then(|entry| entry.width)),
        height: meta.height.or_else(|| entry.and_then(|entry| entry.height)),
        coded_width: entry.and_then(|entry| entry.width),
        coded_height: entry.and_then(|entry| entry.height),
        duration_ms,
        bitrate: entry.and_then(|entry| entry.bitrate),
        fps_num: (fps_num > 0).then_some(fps_num),
        fps_den: (fps_den > 0).then_some(fps_den),
        sample_rate: entry.and_then(|entry| entry.sample_rate),
        channels: entry.and_then(|entry| entry.channels),
        bit_depth: entry.and_then(|entry| entry.bit_depth),
        pixel_format: entry.and_then(|entry| entry.pixel_format.clone()),
        chroma_subsampling: entry.and_then(|entry| entry.chroma_subsampling.clone()),
        color_primaries: entry.and_then(|entry| entry.color_primaries.clone()),
        transfer_characteristics: entry.and_then(|entry| entry.transfer_characteristics.clone()),
        matrix_coefficients: entry.and_then(|entry| entry.matrix_coefficients.clone()),
        rotation_degrees: meta.rotation_degrees,
        details_json: Some(details_json),
    };
    let feature = codec
        .as_deref()
        .map(|codec| codec_feature_for_iso(codec, entry));
    Some(IsoTrackOutput {
        stream_type,
        stream,
        feature,
    })
}

fn parse_mdia(data: &[u8], start: usize, end: usize, meta: &mut IsoTrackMeta) {
    let mut pos = start;
    while let Some(header) = read_box(data, pos, end) {
        match &header.typ {
            b"mdhd" => parse_mdhd(&data[header.data_start..header.end], meta),
            b"hdlr" => {
                let payload = &data[header.data_start..header.end];
                if payload.len() >= 12 {
                    meta.handler = Some(fourcc(&payload[8..12]));
                }
            }
            b"minf" => parse_minf(data, header.data_start, header.end, meta),
            _ => {}
        }
        pos = header.end;
    }
}

fn parse_minf(data: &[u8], start: usize, end: usize, meta: &mut IsoTrackMeta) {
    let mut pos = start;
    while let Some(header) = read_box(data, pos, end) {
        if header.typ == *b"stbl" {
            parse_stbl(data, header.data_start, header.end, meta);
        }
        pos = header.end;
    }
}

fn parse_stbl(data: &[u8], start: usize, end: usize, meta: &mut IsoTrackMeta) {
    let mut pos = start;
    while let Some(header) = read_box(data, pos, end) {
        match &header.typ {
            b"stsd" => parse_stsd(&data[header.data_start..header.end], meta),
            b"stts" => parse_stts(&data[header.data_start..header.end], meta),
            b"stsz" => parse_stsz(&data[header.data_start..header.end], meta),
            _ => {}
        }
        pos = header.end;
    }
}

fn parse_stts(payload: &[u8], meta: &mut IsoTrackMeta) {
    if payload.len() < 16 {
        return;
    }
    let entry_count = BigEndian::read_u32(&payload[4..8]);
    if entry_count == 0 {
        return;
    }
    let sample_count = BigEndian::read_u32(&payload[8..12]) as i64;
    let sample_delta = BigEndian::read_u32(&payload[12..16]) as u64;
    meta.sample_count = Some(sample_count);
    meta.sample_delta = Some(sample_delta);
}

fn parse_stsz(payload: &[u8], meta: &mut IsoTrackMeta) {
    if payload.len() < 12 {
        return;
    }
    let sample_count = BigEndian::read_u32(&payload[8..12]) as i64;
    if sample_count > 0 {
        meta.sample_count = Some(sample_count);
    }
}

fn parse_mvhd(payload: &[u8]) -> (Option<u64>, Option<u64>) {
    if payload.len() < 20 {
        return (None, None);
    }
    let version = payload[0];
    if version == 1 {
        if payload.len() < 32 {
            return (None, None);
        }
        let timescale = BigEndian::read_u32(&payload[20..24]) as u64;
        let duration = BigEndian::read_u64(&payload[24..32]);
        (
            Some(timescale),
            (timescale > 0).then(|| ((duration as u128 * 1000) / timescale as u128) as u64),
        )
    } else {
        let timescale = BigEndian::read_u32(&payload[12..16]) as u64;
        let duration = BigEndian::read_u32(&payload[16..20]) as u64;
        (
            Some(timescale),
            (timescale > 0).then(|| duration.saturating_mul(1000) / timescale),
        )
    }
}

fn parse_mdhd(payload: &[u8], meta: &mut IsoTrackMeta) {
    if payload.len() < 20 {
        return;
    }
    if payload[0] == 1 {
        if payload.len() >= 32 {
            meta.timescale = Some(BigEndian::read_u32(&payload[20..24]) as u64);
            meta.duration = Some(BigEndian::read_u64(&payload[24..32]));
        }
    } else {
        meta.timescale = Some(BigEndian::read_u32(&payload[12..16]) as u64);
        meta.duration = Some(BigEndian::read_u32(&payload[16..20]) as u64);
    }
}

fn parse_tkhd(payload: &[u8], meta: &mut IsoTrackMeta) {
    if payload.len() < 88 {
        return;
    }
    let version = payload[0];
    let (matrix_offset, width_offset) = if version == 1 { (56, 92) } else { (44, 80) };
    if payload.len() >= width_offset + 8 {
        meta.width =
            Some((BigEndian::read_u32(&payload[width_offset..width_offset + 4]) >> 16) as i64);
        meta.height =
            Some((BigEndian::read_u32(&payload[width_offset + 4..width_offset + 8]) >> 16) as i64);
    }
    if payload.len() >= matrix_offset + 32 {
        let a = BigEndian::read_i32(&payload[matrix_offset..matrix_offset + 4]);
        let b = BigEndian::read_i32(&payload[matrix_offset + 4..matrix_offset + 8]);
        let c = BigEndian::read_i32(&payload[matrix_offset + 12..matrix_offset + 16]);
        let d = BigEndian::read_i32(&payload[matrix_offset + 16..matrix_offset + 20]);
        meta.rotation_degrees = rotation_from_matrix(a, b, c, d);
    }
}

fn rotation_from_matrix(a: i32, b: i32, c: i32, d: i32) -> Option<i64> {
    const ONE: i32 = 0x0001_0000;
    if (a, b, c, d) == (ONE, 0, 0, ONE) {
        Some(0)
    } else if (a, b, c, d) == (0, ONE, -ONE, 0) {
        Some(90)
    } else if (a, b, c, d) == (-ONE, 0, 0, -ONE) {
        Some(180)
    } else if (a, b, c, d) == (0, -ONE, ONE, 0) {
        Some(270)
    } else {
        None
    }
}

fn parse_stsd(payload: &[u8], meta: &mut IsoTrackMeta) {
    if payload.len() < 8 {
        return;
    }
    let count = BigEndian::read_u32(&payload[4..8]) as usize;
    let mut pos = 8usize;
    for _ in 0..count {
        if pos + 8 > payload.len() {
            break;
        }
        let size = BigEndian::read_u32(&payload[pos..pos + 4]) as usize;
        if size < 8 || pos + size > payload.len() {
            break;
        }
        let tag = fourcc(&payload[pos + 4..pos + 8]);
        let entry = &payload[pos..pos + size];
        meta.sample_entries.push(parse_sample_entry(&tag, entry));
        pos += size;
    }
}

fn parse_sample_entry(tag: &str, entry: &[u8]) -> IsoSampleEntry {
    let mut width = None;
    let mut height = None;
    let mut channels = None;
    let mut sample_rate = None;
    let mut bit_depth = None;
    let mut bitrate = None;
    let pixel_format = None;
    let mut chroma_subsampling = None;
    let mut color_primaries = None;
    let mut transfer_characteristics = None;
    let mut matrix_coefficients = None;
    let mut profile = None;
    let mut level = None;
    let mut codec = codec_from_sample_entry(tag);
    let nested_offset = if is_video_sample_entry(tag) {
        if entry.len() >= 78 {
            width = Some(BigEndian::read_u16(&entry[32..34]) as i64);
            height = Some(BigEndian::read_u16(&entry[34..36]) as i64);
        }
        86.min(entry.len())
    } else if is_audio_sample_entry(tag) {
        if entry.len() >= 36 {
            channels = Some(BigEndian::read_u16(&entry[24..26]) as i64);
            bit_depth = Some(BigEndian::read_u16(&entry[26..28]) as i64);
            sample_rate = Some((BigEndian::read_u32(&entry[32..36]) >> 16) as i64);
        }
        36.min(entry.len())
    } else {
        8.min(entry.len())
    };
    let mut child_details = Vec::new();
    let mut pos = nested_offset;
    while let Some(header) = read_box(entry, pos, entry.len()) {
        let typ = fourcc(&header.typ);
        let payload = &entry[header.data_start..header.end];
        match typ.as_str() {
            "avcC" if payload.len() >= 4 => {
                profile = Some(payload[1].to_string());
                level = Some(payload[3].to_string());
                child_details.push(json!({
                    "box": typ,
                    "configuration_version": payload[0],
                    "avc_profile_indication": payload[1],
                    "profile_compatibility": payload[2],
                    "avc_level_indication": payload[3],
                }));
            }
            "hvcC" if payload.len() >= 13 => {
                profile = Some((payload[1] & 0x1f).to_string());
                level = Some(payload[12].to_string());
                if payload.len() >= 22 {
                    let chroma_format = payload[16] & 0x03;
                    chroma_subsampling = hevc_chroma_subsampling(chroma_format);
                    bit_depth = Some(((payload[17] & 0x07) + 8) as i64);
                }
                child_details.push(json!({
                    "box": typ,
                    "general_profile_space": payload[1] >> 6,
                    "general_tier_flag": (payload[1] >> 5) & 1,
                    "general_profile_idc": payload[1] & 0x1f,
                    "general_level_idc": payload[12],
                    "chroma_subsampling": chroma_subsampling.clone(),
                    "bit_depth": bit_depth,
                }));
            }
            "av1C" if payload.len() >= 4 => {
                profile = Some(((payload[1] & 0xe0) >> 5).to_string());
                level = Some((payload[1] & 0x1f).to_string());
                bit_depth = Some(if payload[2] & 0x20 != 0 {
                    12
                } else if payload[2] & 0x40 != 0 {
                    10
                } else {
                    8
                });
                chroma_subsampling = Some(if payload[2] & 0x10 != 0 {
                    "monochrome".to_string()
                } else {
                    match ((payload[2] & 0x08) != 0, (payload[2] & 0x04) != 0) {
                        (true, true) => "4:2:0",
                        (true, false) => "4:2:2",
                        (false, false) => "4:4:4",
                        (false, true) => "4:4:0",
                    }
                    .to_string()
                });
                child_details.push(json!({
                    "box": typ,
                    "seq_profile": (payload[1] & 0xe0) >> 5,
                    "seq_level_idx_0": payload[1] & 0x1f,
                    "high_bitdepth": payload[2] & 0x40 != 0,
                    "twelve_bit": payload[2] & 0x20 != 0,
                    "monochrome": payload[2] & 0x10 != 0,
                    "chroma_subsampling_x": payload[2] & 0x08 != 0,
                    "chroma_subsampling_y": payload[2] & 0x04 != 0,
                    "chroma_subsampling": chroma_subsampling.clone(),
                }));
            }
            "esds" => {
                let esds = parse_esds(payload);
                if let Some(esds_codec) = esds.codec.as_deref() {
                    codec = esds_codec.to_string();
                }
                if profile.is_none() {
                    profile = esds.profile.clone();
                }
                if sample_rate.is_none() || sample_rate == Some(0) {
                    sample_rate = esds.sample_rate.map(|value| value as i64);
                }
                if channels.is_none() || channels == Some(0) {
                    channels = esds.channels.map(|value| value as i64);
                }
                bitrate = esds
                    .avg_bitrate
                    .or(esds.max_bitrate)
                    .map(|value| value as i64);
                child_details.push(esds.details);
            }
            "colr" => {
                let color = parse_colr(payload);
                color_primaries = color.primaries.clone();
                transfer_characteristics = color.transfer.clone();
                matrix_coefficients = color.matrix.clone();
                child_details.push(color.details);
            }
            "pasp" if payload.len() >= 8 => {
                child_details.push(json!({
                    "box": typ,
                    "h_spacing": BigEndian::read_u32(&payload[0..4]),
                    "v_spacing": BigEndian::read_u32(&payload[4..8]),
                }));
            }
            "clap" if payload.len() >= 32 => {
                child_details.push(json!({
                    "box": typ,
                    "clean_aperture_width_n": BigEndian::read_u32(&payload[0..4]),
                    "clean_aperture_width_d": BigEndian::read_u32(&payload[4..8]),
                    "clean_aperture_height_n": BigEndian::read_u32(&payload[8..12]),
                    "clean_aperture_height_d": BigEndian::read_u32(&payload[12..16]),
                    "horiz_offset_n": BigEndian::read_u32(&payload[16..20]),
                    "horiz_offset_d": BigEndian::read_u32(&payload[20..24]),
                    "vert_offset_n": BigEndian::read_u32(&payload[24..28]),
                    "vert_offset_d": BigEndian::read_u32(&payload[28..32]),
                }));
            }
            _ => child_details.push(json!({ "box": typ, "size": header.size })),
        }
        pos = header.end;
    }
    IsoSampleEntry {
        tag: tag.to_string(),
        codec,
        width,
        height,
        channels,
        sample_rate,
        bit_depth,
        bitrate,
        pixel_format: pixel_format.clone(),
        chroma_subsampling: chroma_subsampling.clone(),
        color_primaries: color_primaries.clone(),
        transfer_characteristics: transfer_characteristics.clone(),
        matrix_coefficients: matrix_coefficients.clone(),
        profile,
        level,
        details: json!({
            "tag": tag,
            "width": width,
            "height": height,
            "channels": channels,
            "sample_rate": sample_rate,
            "bit_depth": bit_depth,
            "bitrate": bitrate,
            "pixel_format": pixel_format,
            "chroma_subsampling": chroma_subsampling,
            "color_primaries": color_primaries,
            "transfer_characteristics": transfer_characteristics,
            "matrix_coefficients": matrix_coefficients,
            "boxes": child_details,
        }),
    }
}

#[derive(Default)]
struct EsdsInfo {
    codec: Option<String>,
    profile: Option<String>,
    object_type: Option<u8>,
    stream_type: Option<u8>,
    max_bitrate: Option<u32>,
    avg_bitrate: Option<u32>,
    audio_object_type: Option<u8>,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    details: Value,
}

fn parse_esds(payload: &[u8]) -> EsdsInfo {
    let mut info = EsdsInfo::default();
    let descriptor_data = if payload.len() >= 4 {
        &payload[4..]
    } else {
        payload
    };
    parse_es_descriptors(descriptor_data, 0, &mut info);
    info.codec = info
        .object_type
        .and_then(codec_from_object_type)
        .map(str::to_string);
    if info.codec.as_deref() == Some("aac") {
        info.profile = info
            .audio_object_type
            .map(|value| aac_profile(u32::from(value)))
            .or_else(|| Some("aac".to_string()));
    }
    info.details = json!({
        "box": "esds",
        "object_type_indication": info.object_type,
        "stream_type": info.stream_type,
        "codec": info.codec.clone(),
        "profile": info.profile.clone(),
        "max_bitrate": info.max_bitrate,
        "avg_bitrate": info.avg_bitrate,
        "audio_object_type": info.audio_object_type,
        "sample_rate": info.sample_rate,
        "channels": info.channels,
    });
    info
}

fn parse_es_descriptors(mut data: &[u8], depth: usize, info: &mut EsdsInfo) {
    if depth >= 8 {
        return;
    }
    while data.len() >= 2 {
        let tag = data[0];
        let Some((len, len_bytes)) = read_descriptor_len(&data[1..]) else {
            return;
        };
        let header_len = 1 + len_bytes;
        if header_len + len > data.len() {
            return;
        }
        let payload = &data[header_len..header_len + len];
        match tag {
            0x03 => {
                let mut nested = 3usize;
                if payload.len() >= 3 {
                    let flags = payload[2];
                    if flags & 0x80 != 0 {
                        nested = nested.saturating_add(2);
                    }
                    if flags & 0x40 != 0 && nested < payload.len() {
                        nested = nested.saturating_add(1 + payload[nested] as usize);
                    }
                    if flags & 0x20 != 0 {
                        nested = nested.saturating_add(2);
                    }
                }
                if nested < payload.len() {
                    parse_es_descriptors(&payload[nested..], depth + 1, info);
                }
            }
            0x04 => {
                if payload.len() >= 13 {
                    info.object_type = Some(payload[0]);
                    info.stream_type = Some(payload[1] >> 2);
                    info.max_bitrate = Some(BigEndian::read_u32(&payload[5..9]));
                    info.avg_bitrate = Some(BigEndian::read_u32(&payload[9..13]));
                    parse_es_descriptors(&payload[13..], depth + 1, info);
                }
            }
            0x05 => parse_audio_specific_config(payload, info),
            _ => {}
        }
        data = &data[header_len + len..];
    }
}

fn read_descriptor_len(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for (index, byte) in data.iter().take(4).enumerate() {
        value = (value << 7) | usize::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

fn parse_audio_specific_config(data: &[u8], info: &mut EsdsInfo) {
    let mut bit_pos = 0usize;
    let Some(mut audio_object_type) = read_bits(data, &mut bit_pos, 5) else {
        return;
    };
    if audio_object_type == 31 {
        let Some(ext) = read_bits(data, &mut bit_pos, 6) else {
            return;
        };
        audio_object_type = 32 + ext;
    }
    let Some(freq_index) = read_bits(data, &mut bit_pos, 4) else {
        return;
    };
    let sample_rate = if freq_index == 15 {
        read_bits(data, &mut bit_pos, 24)
    } else {
        aac_sample_rate(freq_index as u8)
    };
    let channels = read_bits(data, &mut bit_pos, 4).map(|value| value as u8);
    info.audio_object_type = Some(audio_object_type as u8);
    info.sample_rate = sample_rate;
    info.channels = channels;
}

fn read_bits(data: &[u8], bit_pos: &mut usize, bits: usize) -> Option<u32> {
    if bits > 32 || *bit_pos + bits > data.len().saturating_mul(8) {
        return None;
    }
    let mut value = 0u32;
    for _ in 0..bits {
        let byte = data[*bit_pos / 8];
        let shift = 7 - (*bit_pos % 8);
        value = (value << 1) | u32::from((byte >> shift) & 1);
        *bit_pos += 1;
    }
    Some(value)
}

fn codec_from_object_type(object_type: u8) -> Option<&'static str> {
    match object_type {
        0x20 => Some("mpeg4-video"),
        0x21 => Some("h264"),
        0x23 => Some("h265"),
        0x40 | 0x66 | 0x67 | 0x68 => Some("aac"),
        0x69 | 0x6b => Some("mp3"),
        0x6c => Some("jpeg"),
        0xa3 => Some("vc1"),
        _ => None,
    }
}

fn aac_sample_rate(index: u8) -> Option<u32> {
    match index {
        0 => Some(96_000),
        1 => Some(88_200),
        2 => Some(64_000),
        3 => Some(48_000),
        4 => Some(44_100),
        5 => Some(32_000),
        6 => Some(24_000),
        7 => Some(22_050),
        8 => Some(16_000),
        9 => Some(12_000),
        10 => Some(11_025),
        11 => Some(8_000),
        12 => Some(7_350),
        _ => None,
    }
}

fn aac_profile(audio_object_type: u32) -> String {
    match audio_object_type {
        1 => "aac-main",
        2 => "aac-lc",
        3 => "aac-ssr",
        4 => "aac-ltp",
        5 => "he-aac",
        23 => "aac-ld",
        29 => "he-aac-v2",
        39 => "aac-eld",
        _ => "aac",
    }
    .to_string()
}

struct ColorInfo {
    primaries: Option<String>,
    transfer: Option<String>,
    matrix: Option<String>,
    details: Value,
}

fn parse_colr(payload: &[u8]) -> ColorInfo {
    let color_type = (payload.len() >= 4).then(|| fourcc(&payload[0..4]));
    let mut primaries = None;
    let mut transfer = None;
    let mut matrix = None;
    let mut full_range = None;
    if matches!(color_type.as_deref(), Some("nclx" | "nclc")) && payload.len() >= 10 {
        let primaries_id = BigEndian::read_u16(&payload[4..6]);
        let transfer_id = BigEndian::read_u16(&payload[6..8]);
        let matrix_id = BigEndian::read_u16(&payload[8..10]);
        primaries = Some(color_primaries_name(primaries_id).to_string());
        transfer = Some(transfer_characteristics_name(transfer_id).to_string());
        matrix = Some(matrix_coefficients_name(matrix_id).to_string());
        if color_type.as_deref() == Some("nclx") && payload.len() >= 11 {
            full_range = Some(payload[10] & 0x80 != 0);
        }
    }
    ColorInfo {
        primaries: primaries.clone(),
        transfer: transfer.clone(),
        matrix: matrix.clone(),
        details: json!({
            "box": "colr",
            "color_type": color_type,
            "color_primaries": primaries,
            "transfer_characteristics": transfer,
            "matrix_coefficients": matrix,
            "full_range": full_range,
        }),
    }
}

fn color_primaries_name(value: u16) -> &'static str {
    match value {
        1 => "bt709",
        4 => "bt470m",
        5 => "bt470bg",
        6 => "smpte170m",
        9 => "bt2020",
        12 => "display-p3",
        _ => "unknown",
    }
}

fn transfer_characteristics_name(value: u16) -> &'static str {
    match value {
        1 => "bt709",
        6 => "smpte170m",
        13 => "srgb",
        16 => "pq",
        18 => "hlg",
        _ => "unknown",
    }
}

fn matrix_coefficients_name(value: u16) -> &'static str {
    match value {
        0 => "rgb",
        1 => "bt709",
        5 => "bt470bg",
        6 => "smpte170m",
        9 => "bt2020_ncl",
        10 => "bt2020_cl",
        _ => "unknown",
    }
}

fn hevc_chroma_subsampling(chroma_format: u8) -> Option<String> {
    match chroma_format {
        0 => Some("monochrome".to_string()),
        1 => Some("4:2:0".to_string()),
        2 => Some("4:2:2".to_string()),
        3 => Some("4:4:4".to_string()),
        _ => None,
    }
}

fn is_video_sample_entry(tag: &str) -> bool {
    matches!(
        tag,
        "avc1" | "avc3" | "hvc1" | "hev1" | "av01" | "mp4v" | "jpeg" | "mjpg" | "png " | "jxlc"
    )
}

fn is_audio_sample_entry(tag: &str) -> bool {
    matches!(
        tag,
        "mp4a" | "alac" | "Opus" | "fLaC" | "samr" | "sawb" | "enca"
    )
}

fn codec_from_sample_entry(tag: &str) -> String {
    match tag {
        "avc1" | "avc3" => "h264",
        "hvc1" | "hev1" => "h265",
        "av01" => "av1",
        "mp4a" => "aac",
        "alac" => "alac",
        "mp4v" => "mpeg4-video",
        "jpeg" | "mjpg" => "jpeg",
        "png " => "png",
        "jxlc" => "jpegxl",
        "Opus" => "opus",
        "fLaC" => "flac",
        other => other,
    }
    .to_string()
}

fn codec_feature_for_iso(codec: &str, entry: Option<&IsoSampleEntry>) -> MediaCodecFeatureRecord {
    let details = entry
        .map(|entry| entry.details.clone())
        .unwrap_or_else(|| json!({ "codec": codec }));
    let (features_json, _) = json_limited(&details);
    MediaCodecFeatureRecord {
        stream_index: 0,
        standard: Some(codec.to_string()),
        coding_mode: Some("container_header".to_string()),
        confidence: Some("container".to_string()),
        features_json: Some(features_json),
        ..MediaCodecFeatureRecord::default()
    }
}

fn iso_format(major_brand: Option<&str>, brands: &[String]) -> String {
    let has_brand =
        |wanted: &str| major_brand == Some(wanted) || brands.iter().any(|brand| brand == wanted);
    if has_brand("avif") || has_brand("avis") {
        "avif"
    } else if has_brand("heic") || has_brand("heix") || has_brand("hevc") {
        "heic"
    } else if has_brand("heif") || has_brand("mif1") || has_brand("msf1") {
        "heif"
    } else if has_brand("M4A ") || has_brand("M4B ") {
        "m4a"
    } else if major_brand
        .map(|brand| brand.starts_with("3g"))
        .unwrap_or(false)
    {
        "3gp"
    } else if has_brand("qt  ") {
        "mov"
    } else {
        "mp4"
    }
    .to_string()
}

fn parse_riff(data: &[u8], file_size: u64) -> ParsedMedia {
    let riff_type = if data.len() >= 12 {
        fourcc(&data[8..12])
    } else {
        "unknown".to_string()
    };
    if riff_type == "WEBP" {
        return parse_webp_with_file_size(data, file_size);
    }
    if riff_type == "WAVE" {
        return parse_wav(data, file_size);
    }
    if riff_type == "AVI " {
        return ParsedMedia {
            media_kind: "video".to_string(),
            container: "riff".to_string(),
            detected_format: "avi".to_string(),
            mime: Some("video/x-msvideo".to_string()),
            scan_status: "partial".to_string(),
            error: Some("AVI RIFF metadata parser is shallow".to_string()),
            streams: vec![MediaStreamRecord {
                stream_index: 0,
                stream_type: "video".to_string(),
                codec: Some("avi".to_string()),
                details_json: Some("{\"riff_type\":\"AVI \"}".to_string()),
                ..MediaStreamRecord::default()
            }],
            features: Vec::new(),
        };
    }
    ParsedMedia {
        media_kind: "unknown".to_string(),
        container: "riff".to_string(),
        detected_format: riff_type,
        mime: None,
        scan_status: "unsupported".to_string(),
        error: Some("unsupported RIFF type".to_string()),
        streams: Vec::new(),
        features: Vec::new(),
    }
}

fn parse_wav(data: &[u8], file_size: u64) -> ParsedMedia {
    let mut format_tag = None;
    let mut channels = None;
    let mut sample_rate = None;
    let mut byte_rate = None;
    let mut bit_depth = None;
    let mut data_size = None;
    let mut pos = 12usize;
    let mut partial = false;
    let mut truncated_by_probe = false;
    while pos + 8 <= data.len() {
        let typ = fourcc(&data[pos..pos + 4]);
        let len = LittleEndian::read_u32(&data[pos + 4..pos + 8]) as usize;
        if typ == "data" {
            data_size = Some(len as i64);
        }
        if pos + 8 + len > data.len() {
            partial = true;
            truncated_by_probe = true;
            break;
        }
        let chunk = &data[pos + 8..pos + 8 + len];
        match typ.as_str() {
            "fmt " if chunk.len() >= 16 => {
                format_tag = Some(LittleEndian::read_u16(&chunk[0..2]) as i64);
                channels = Some(LittleEndian::read_u16(&chunk[2..4]) as i64);
                sample_rate = Some(LittleEndian::read_u32(&chunk[4..8]) as i64);
                byte_rate = Some(LittleEndian::read_u32(&chunk[8..12]) as i64);
                bit_depth = Some(LittleEndian::read_u16(&chunk[14..16]) as i64);
            }
            "data" => {}
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    let duration_ms = match (data_size, byte_rate) {
        (Some(size), Some(rate)) if rate > 0 => Some(size.saturating_mul(1000) / rate),
        _ => None,
    };
    let probe_truncated = partial
        && truncated_by_probe
        && file_size > data.len() as u64
        && format_tag.is_some()
        && sample_rate.is_some();
    let details = json!({
        "format_tag": format_tag,
        "data_size": data_size,
        "byte_rate": byte_rate,
        "probe_truncated": probe_truncated,
        "probe_read_bytes": data.len(),
        "file_size": file_size,
    });
    ParsedMedia {
        media_kind: "audio".to_string(),
        container: "riff".to_string(),
        detected_format: "wav".to_string(),
        mime: Some("audio/wav".to_string()),
        scan_status: if partial && !probe_truncated {
            "partial"
        } else {
            "ok"
        }
        .to_string(),
        error: (partial && !probe_truncated).then(|| "truncated WAV chunk".to_string()),
        streams: vec![MediaStreamRecord {
            stream_index: 0,
            stream_type: "audio".to_string(),
            codec: format_tag.map(wav_codec),
            duration_ms,
            sample_rate,
            channels,
            bit_depth,
            bitrate: byte_rate.map(|rate| rate * 8),
            details_json: Some(serde_json::to_string(&details).unwrap_or_default()),
            ..MediaStreamRecord::default()
        }],
        features: Vec::new(),
    }
}

fn wav_codec(format_tag: i64) -> String {
    match format_tag {
        1 => "pcm",
        3 => "float",
        6 => "alaw",
        7 => "mulaw",
        0x55 => "mp3",
        0xfffe => "extensible",
        _ => "unknown",
    }
    .to_string()
}

fn parse_silk(data: &[u8], file_size: u64) -> ParsedMedia {
    let header_offset = silk_header_offset(data).unwrap_or(0);
    let header_end = header_offset + b"#!SILK_V3".len();
    let control_byte = data.get(header_end).copied();
    let details = json!({
        "header_offset": header_offset,
        "control_byte": control_byte,
        "probe_read_bytes": data.len(),
        "file_size": file_size,
    });
    let (details_json, details_partial) = json_limited(&details);
    ParsedMedia {
        media_kind: "audio".to_string(),
        container: "silk".to_string(),
        detected_format: "silk".to_string(),
        mime: Some("audio/silk".to_string()),
        scan_status: if details_partial { "partial" } else { "ok" }.to_string(),
        error: None,
        streams: vec![MediaStreamRecord {
            stream_index: 0,
            stream_type: "audio".to_string(),
            codec: Some("silk".to_string()),
            details_json: Some(details_json),
            ..MediaStreamRecord::default()
        }],
        features: vec![MediaCodecFeatureRecord {
            stream_index: 0,
            standard: Some("silk".to_string()),
            coding_mode: Some("speech".to_string()),
            confidence: Some("header".to_string()),
            features_json: Some(
                serde_json::to_string(&json!({
                    "header_offset": header_offset,
                    "control_byte": control_byte,
                }))
                .unwrap_or_else(|_| "{}".to_string()),
            ),
            ..MediaCodecFeatureRecord::default()
        }],
    }
}

fn parse_svg(data: &[u8]) -> ParsedMedia {
    let text = String::from_utf8_lossy(&data[..data.len().min(4096)]);
    let width = svg_dimension_attr(&text, "width");
    let height = svg_dimension_attr(&text, "height");
    let details = json!({
        "root": "svg",
        "width_attr": width,
        "height_attr": height,
    });
    let (details_json, details_partial) = json_limited(&details);
    ParsedMedia {
        media_kind: "image_vector".to_string(),
        container: "svg".to_string(),
        detected_format: "svg".to_string(),
        mime: Some("image/svg+xml".to_string()),
        scan_status: if details_partial { "partial" } else { "ok" }.to_string(),
        error: None,
        streams: vec![MediaStreamRecord {
            stream_index: 0,
            stream_type: "image".to_string(),
            codec: Some("svg".to_string()),
            width,
            height,
            details_json: Some(details_json),
            ..MediaStreamRecord::default()
        }],
        features: vec![MediaCodecFeatureRecord {
            stream_index: 0,
            standard: Some("svg".to_string()),
            coding_mode: Some("xml_vector".to_string()),
            lossless: Some(1),
            confidence: Some("header".to_string()),
            features_json: Some("{\"root\":\"svg\"}".to_string()),
            ..MediaCodecFeatureRecord::default()
        }],
    }
}

fn svg_dimension_attr(text: &str, attr: &str) -> Option<i64> {
    for quote in ['"', '\''] {
        let needle = format!("{attr}={quote}");
        let Some(start) = text.find(&needle) else {
            continue;
        };
        let value_start = start + needle.len();
        let value = &text[value_start..];
        let value_end = value.find(quote).unwrap_or(value.len());
        let numeric = value[..value_end]
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
            .collect::<String>();
        if let Ok(parsed) = numeric.parse::<f64>() {
            return Some(parsed.round() as i64);
        }
    }
    None
}

const MAX_MP3_SYNC_SEARCH_BYTES: usize = 64 * 1024;
const MAX_MP3_SYNC_CANDIDATES: usize = 128;
const MIN_MP3_CONSISTENT_FRAMES: usize = 3;
const MAX_MP3_CONSISTENCY_FRAMES: usize = 8;

#[derive(Clone, Debug)]
struct Id3v2Info {
    version: u8,
    revision: u8,
    flags: u8,
    tag_size: usize,
    total_size: usize,
    footer: bool,
}

#[derive(Clone, Debug)]
struct Mp3FrameHeader {
    version_id: u8,
    layer_index: u8,
    protection_absent: bool,
    bitrate_index: usize,
    sample_rate_index: usize,
    padding: bool,
    channel_mode: u8,
    bitrate_bps: u32,
    sample_rate: u32,
    frame_len: usize,
    samples_per_frame: u16,
}

#[derive(Clone, Debug)]
struct Mp3VbrInfo {
    marker: String,
    offset: usize,
    frames: Option<u32>,
    bytes: Option<u32>,
    quality: Option<u32>,
    version: Option<u16>,
    delay: Option<u16>,
}

#[derive(Clone, Debug)]
struct Mp3Probe {
    id3v2: Option<Id3v2Info>,
    first_frame_offset: usize,
    first_frame: Mp3FrameHeader,
    checked_frames: usize,
    frame_bytes_checked: usize,
    bitrate_changes: usize,
    vbr: Option<Mp3VbrInfo>,
}

fn parse_mp3(data: &[u8], file_size: u64) -> ParsedMedia {
    let probe = find_mp3_probe(data);
    let mut stream = MediaStreamRecord {
        stream_index: 0,
        stream_type: "audio".to_string(),
        codec: Some("mp3".to_string()),
        ..MediaStreamRecord::default()
    };
    let mut details = json!({});
    if let Some(probe) = &probe {
        let frame = &probe.first_frame;
        stream.bitrate = mp3_effective_bitrate(probe).or(Some(frame.bitrate_bps as i64));
        stream.sample_rate = Some(frame.sample_rate as i64);
        stream.channels = Some(frame.channels() as i64);
        stream.profile = Some(frame.layer_name().to_string());
        stream.duration_ms = mp3_duration_ms(file_size, probe);
        details = json!({
            "id3v2": probe.id3v2.as_ref().map(id3v2_details),
            "mpeg_version": frame.version_name(),
            "layer": frame.layer_name(),
            "channel_mode": frame.channel_mode,
            "bitrate_index": frame.bitrate_index,
            "sample_rate_index": frame.sample_rate_index,
            "padding": frame.padding,
            "protection_absent": frame.protection_absent,
            "first_frame_offset": probe.first_frame_offset,
            "first_frame_len": frame.frame_len,
            "samples_per_frame": frame.samples_per_frame,
            "frame_consistency": {
                "checked_frames": probe.checked_frames,
                "required_frames": MIN_MP3_CONSISTENT_FRAMES,
                "frame_bytes_checked": probe.frame_bytes_checked,
                "bitrate_changes": probe.bitrate_changes,
                "result": "ok",
            },
            "vbr": probe.vbr.as_ref().map(mp3_vbr_details),
        });
    }
    stream.details_json = Some(serde_json::to_string(&details).unwrap_or_default());
    ParsedMedia {
        media_kind: "audio".to_string(),
        container: "mp3".to_string(),
        detected_format: "mp3".to_string(),
        mime: Some("audio/mpeg".to_string()),
        scan_status: if probe.is_some() { "ok" } else { "error" }.to_string(),
        error: probe
            .is_none()
            .then(|| "valid MP3 frame sequence not found".to_string()),
        streams: vec![stream],
        features: Vec::new(),
    }
}

fn id3v2_details(info: &Id3v2Info) -> Value {
    json!({
        "version": info.version,
        "revision": info.revision,
        "flags": info.flags,
        "tag_size": info.tag_size,
        "total_size": info.total_size,
        "footer": info.footer,
    })
}

fn mp3_vbr_details(info: &Mp3VbrInfo) -> Value {
    json!({
        "marker": info.marker,
        "offset": info.offset,
        "frames": info.frames,
        "bytes": info.bytes,
        "quality": info.quality,
        "version": info.version,
        "delay": info.delay,
    })
}

fn mp3_effective_bitrate(probe: &Mp3Probe) -> Option<i64> {
    let vbr = probe.vbr.as_ref()?;
    let frames = vbr.frames?;
    let bytes = vbr.bytes?;
    if frames == 0 || bytes == 0 || probe.first_frame.sample_rate == 0 {
        return None;
    }
    let duration_num = frames as u128 * probe.first_frame.samples_per_frame as u128;
    let duration_den = probe.first_frame.sample_rate as u128;
    if duration_num == 0 {
        return None;
    }
    Some(((bytes as u128 * 8 * duration_den) / duration_num) as i64)
}

fn mp3_duration_ms(file_size: u64, probe: &Mp3Probe) -> Option<i64> {
    if let Some(frames) = probe.vbr.as_ref().and_then(|vbr| vbr.frames) {
        if probe.first_frame.sample_rate > 0 {
            return Some(
                ((frames as u128 * probe.first_frame.samples_per_frame as u128 * 1000)
                    / probe.first_frame.sample_rate as u128) as i64,
            );
        }
    }
    let audio_bytes = file_size.saturating_sub(
        probe
            .id3v2
            .as_ref()
            .map(|id3| id3.total_size as u64)
            .unwrap_or(0),
    );
    (probe.first_frame.bitrate_bps > 0)
        .then(|| ((audio_bytes as u128 * 8 * 1000) / probe.first_frame.bitrate_bps as u128) as i64)
}

fn find_mp3_probe(data: &[u8]) -> Option<Mp3Probe> {
    let id3v2 = parse_id3v2(data);
    let search_start = id3v2
        .as_ref()
        .map(|id3| id3.total_size)
        .unwrap_or(0)
        .min(data.len());
    let search_end = data
        .len()
        .min(search_start.saturating_add(MAX_MP3_SYNC_SEARCH_BYTES));
    let mut candidates = 0usize;
    let mut pos = search_start;
    while pos + 4 <= search_end {
        if data[pos] == 0xff && data[pos + 1] & 0xe0 == 0xe0 {
            candidates += 1;
            if candidates > MAX_MP3_SYNC_CANDIDATES {
                break;
            }
            if let Some(probe) = validate_mp3_frame_sequence(data, id3v2.clone(), pos) {
                return Some(probe);
            }
        }
        pos += 1;
    }
    None
}

fn parse_id3v2(data: &[u8]) -> Option<Id3v2Info> {
    if data.len() < 10 || !data.starts_with(b"ID3") {
        return None;
    }
    let version = data[3];
    let revision = data[4];
    if !(2..=4).contains(&version)
        || version == 0xff
        || revision == 0xff
        || data[6..10].iter().any(|byte| byte & 0x80 != 0)
    {
        return None;
    }
    let tag_size = synchsafe_u32(&data[6..10]) as usize;
    let footer = version == 4 && data[5] & 0x10 != 0;
    let total_size = 10usize
        .checked_add(tag_size)?
        .checked_add(if footer { 10 } else { 0 })?;
    Some(Id3v2Info {
        version,
        revision,
        flags: data[5],
        tag_size,
        total_size,
        footer,
    })
}

fn validate_mp3_frame_sequence(
    data: &[u8],
    id3v2: Option<Id3v2Info>,
    first_frame_offset: usize,
) -> Option<Mp3Probe> {
    let first_frame = parse_mp3_frame_header(data, first_frame_offset)?;
    let mut pos = first_frame_offset;
    let mut checked_frames = 0usize;
    let mut frame_bytes_checked = 0usize;
    let mut bitrate_changes = 0usize;
    while checked_frames < MAX_MP3_CONSISTENCY_FRAMES && pos + 4 <= data.len() {
        let Some(frame) = parse_mp3_frame_header(data, pos) else {
            break;
        };
        if !frame.is_consistent_with(&first_frame) {
            break;
        }
        if checked_frames > 0 && frame.bitrate_index != first_frame.bitrate_index {
            bitrate_changes += 1;
        }
        let next = pos.checked_add(frame.frame_len)?;
        if next > data.len() {
            break;
        }
        checked_frames += 1;
        frame_bytes_checked = next.saturating_sub(first_frame_offset);
        pos = next;
    }
    if checked_frames < MIN_MP3_CONSISTENT_FRAMES {
        return None;
    }
    let vbr = detect_mp3_vbr(data, first_frame_offset, &first_frame);
    Some(Mp3Probe {
        id3v2,
        first_frame_offset,
        first_frame,
        checked_frames,
        frame_bytes_checked,
        bitrate_changes,
        vbr,
    })
}

fn parse_mp3_frame_header(data: &[u8], pos: usize) -> Option<Mp3FrameHeader> {
    if pos + 4 > data.len() {
        return None;
    }
    let header = BigEndian::read_u32(&data[pos..pos + 4]);
    if header & 0xffe0_0000 != 0xffe0_0000 {
        return None;
    }
    let version_id = ((header >> 19) & 0x3) as u8;
    let layer_index = ((header >> 17) & 0x3) as u8;
    let protection_absent = ((header >> 16) & 0x1) != 0;
    let bitrate_index = ((header >> 12) & 0xf) as usize;
    let sample_rate_index = ((header >> 10) & 0x3) as usize;
    let padding = ((header >> 9) & 0x1) != 0;
    let channel_mode = ((header >> 6) & 0x3) as u8;
    let emphasis = (header & 0x3) as u8;
    if version_id == 1 || layer_index == 0 || emphasis == 2 {
        return None;
    }
    let bitrate_bps = mp3_bitrate_kbps(version_id, layer_index, bitrate_index)? as u32 * 1000;
    let sample_rate = mp3_sample_rate(version_id, sample_rate_index)?;
    let frame_len = mp3_frame_len(version_id, layer_index, bitrate_bps, sample_rate, padding)?;
    let samples_per_frame = mp3_samples_per_frame(version_id, layer_index)?;
    Some(Mp3FrameHeader {
        version_id,
        layer_index,
        protection_absent,
        bitrate_index,
        sample_rate_index,
        padding,
        channel_mode,
        bitrate_bps,
        sample_rate,
        frame_len,
        samples_per_frame,
    })
}

fn mp3_bitrate_kbps(version_id: u8, layer_index: u8, index: usize) -> Option<u16> {
    if index == 0 || index == 15 {
        return None;
    }
    const MPEG1_L1: [u16; 16] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
    ];
    const MPEG1_L2: [u16; 16] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
    ];
    const MPEG1_L3: [u16; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    const MPEG2_L1: [u16; 16] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
    ];
    const MPEG2_L2L3: [u16; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    let table = match (version_id == 3, layer_index) {
        (true, 3) => &MPEG1_L1,
        (true, 2) => &MPEG1_L2,
        (true, 1) => &MPEG1_L3,
        (false, 3) => &MPEG2_L1,
        (false, 2 | 1) => &MPEG2_L2L3,
        _ => return None,
    };
    Some(table[index])
}

fn mp3_sample_rate(version_id: u8, index: usize) -> Option<u32> {
    if index == 3 {
        return None;
    }
    let base = [44100, 48000, 32000][index];
    match version_id {
        3 => Some(base),
        2 => Some(base / 2),
        0 => Some(base / 4),
        _ => None,
    }
}

fn mp3_frame_len(
    version_id: u8,
    layer_index: u8,
    bitrate_bps: u32,
    sample_rate: u32,
    padding: bool,
) -> Option<usize> {
    if sample_rate == 0 {
        return None;
    }
    let padding = u32::from(padding);
    let len = match layer_index {
        3 => (((12 * bitrate_bps) / sample_rate) + padding) * 4,
        2 => ((144 * bitrate_bps) / sample_rate) + padding,
        1 if version_id == 3 => ((144 * bitrate_bps) / sample_rate) + padding,
        1 => ((72 * bitrate_bps) / sample_rate) + padding,
        _ => return None,
    };
    (len >= 4).then_some(len as usize)
}

fn mp3_samples_per_frame(version_id: u8, layer_index: u8) -> Option<u16> {
    match layer_index {
        3 => Some(384),
        2 => Some(1152),
        1 if version_id == 3 => Some(1152),
        1 => Some(576),
        _ => None,
    }
}

impl Mp3FrameHeader {
    fn version_name(&self) -> &'static str {
        match self.version_id {
            0 => "mpeg2.5",
            2 => "mpeg2",
            3 => "mpeg1",
            _ => "reserved",
        }
    }

    fn layer_name(&self) -> &'static str {
        match self.layer_index {
            1 => "layer3",
            2 => "layer2",
            3 => "layer1",
            _ => "reserved",
        }
    }

    fn channels(&self) -> u8 {
        if self.channel_mode == 3 {
            1
        } else {
            2
        }
    }

    fn is_consistent_with(&self, first: &Self) -> bool {
        self.version_id == first.version_id
            && self.layer_index == first.layer_index
            && self.sample_rate == first.sample_rate
    }
}

fn detect_mp3_vbr(data: &[u8], frame_offset: usize, frame: &Mp3FrameHeader) -> Option<Mp3VbrInfo> {
    if let Some(offset) = mp3_xing_offset(frame_offset, frame) {
        if let Some(info) = parse_xing_info(data, offset) {
            return Some(info);
        }
    }
    if let Some(info) = parse_vbri_info(data, frame_offset.saturating_add(36)) {
        return Some(info);
    }
    let start = frame_offset.saturating_add(4);
    let end = data.len().min(
        frame_offset
            .saturating_add(frame.frame_len)
            .min(frame_offset + 256),
    );
    if start >= end {
        return None;
    }
    let window = &data[start..end];
    for (rel, marker) in window.windows(4).enumerate() {
        let offset = start + rel;
        match marker {
            b"Xing" | b"Info" => {
                if let Some(info) = parse_xing_info(data, offset) {
                    return Some(info);
                }
            }
            b"VBRI" => {
                if let Some(info) = parse_vbri_info(data, offset) {
                    return Some(info);
                }
            }
            _ => {}
        }
    }
    None
}

fn mp3_xing_offset(frame_offset: usize, frame: &Mp3FrameHeader) -> Option<usize> {
    if frame.layer_index != 1 {
        return None;
    }
    let side_info_len = match (frame.version_id == 3, frame.channels()) {
        (true, 1) => 17,
        (true, _) => 32,
        (false, 1) => 9,
        (false, _) => 17,
    };
    Some(
        frame_offset
            .saturating_add(4)
            .saturating_add(if frame.protection_absent { 0 } else { 2 })
            .saturating_add(side_info_len),
    )
}

fn parse_xing_info(data: &[u8], offset: usize) -> Option<Mp3VbrInfo> {
    if offset + 8 > data.len() {
        return None;
    }
    let marker = match &data[offset..offset + 4] {
        b"Xing" => "xing",
        b"Info" => "info",
        _ => return None,
    };
    let flags = BigEndian::read_u32(&data[offset + 4..offset + 8]);
    let mut cursor = offset + 8;
    let frames = if flags & 0x1 != 0 {
        let value = read_mp3_optional_u32(data, &mut cursor)?;
        Some(value)
    } else {
        None
    };
    let bytes = if flags & 0x2 != 0 {
        let value = read_mp3_optional_u32(data, &mut cursor)?;
        Some(value)
    } else {
        None
    };
    if flags & 0x4 != 0 {
        cursor = cursor.checked_add(100)?;
        if cursor > data.len() {
            return None;
        }
    }
    let quality = if flags & 0x8 != 0 {
        let value = read_mp3_optional_u32(data, &mut cursor)?;
        Some(value)
    } else {
        None
    };
    Some(Mp3VbrInfo {
        marker: marker.to_string(),
        offset,
        frames,
        bytes,
        quality,
        version: None,
        delay: None,
    })
}

fn parse_vbri_info(data: &[u8], offset: usize) -> Option<Mp3VbrInfo> {
    if offset + 18 > data.len() || &data[offset..offset + 4] != b"VBRI" {
        return None;
    }
    Some(Mp3VbrInfo {
        marker: "vbri".to_string(),
        offset,
        version: Some(BigEndian::read_u16(&data[offset + 4..offset + 6])),
        delay: Some(BigEndian::read_u16(&data[offset + 6..offset + 8])),
        quality: Some(BigEndian::read_u16(&data[offset + 8..offset + 10]) as u32),
        bytes: Some(BigEndian::read_u32(&data[offset + 10..offset + 14])),
        frames: Some(BigEndian::read_u32(&data[offset + 14..offset + 18])),
    })
}

fn read_mp3_optional_u32(data: &[u8], cursor: &mut usize) -> Option<u32> {
    if *cursor + 4 > data.len() {
        return None;
    }
    let value = BigEndian::read_u32(&data[*cursor..*cursor + 4]);
    *cursor += 4;
    Some(value)
}

fn parse_flac(data: &[u8]) -> ParsedMedia {
    let mut pos = 4usize;
    let mut stream = MediaStreamRecord {
        stream_index: 0,
        stream_type: "audio".to_string(),
        codec: Some("flac".to_string()),
        ..MediaStreamRecord::default()
    };
    let mut partial = false;
    while pos + 4 <= data.len() {
        let header = data[pos];
        let block_type = header & 0x7f;
        let last = header & 0x80 != 0;
        let len = read_u24be(&data[pos + 1..pos + 4]) as usize;
        pos += 4;
        if pos + len > data.len() {
            partial = true;
            break;
        }
        if block_type == 0 && len >= 34 {
            let block = &data[pos..pos + len];
            let packed = BigEndian::read_u64(&block[10..18]);
            let sample_rate = ((packed >> 44) & 0x000f_ffff) as i64;
            let channels = (((packed >> 41) & 0x7) + 1) as i64;
            let bit_depth = (((packed >> 36) & 0x1f) + 1) as i64;
            let total_samples = (packed & 0x000f_ffff_ffff) as i64;
            stream.sample_rate = Some(sample_rate);
            stream.channels = Some(channels);
            stream.bit_depth = Some(bit_depth);
            if sample_rate > 0 && total_samples > 0 {
                stream.duration_ms = Some(total_samples.saturating_mul(1000) / sample_rate);
            }
            stream.details_json = Some(
                serde_json::to_string(&json!({
                    "min_block_size": BigEndian::read_u16(&block[0..2]),
                    "max_block_size": BigEndian::read_u16(&block[2..4]),
                    "total_samples": total_samples,
                }))
                .unwrap_or_default(),
            );
        }
        pos += len;
        if last {
            break;
        }
    }
    ParsedMedia {
        media_kind: "audio".to_string(),
        container: "flac".to_string(),
        detected_format: "flac".to_string(),
        mime: Some("audio/flac".to_string()),
        scan_status: if partial { "partial" } else { "ok" }.to_string(),
        error: partial.then(|| "truncated FLAC metadata block".to_string()),
        streams: vec![stream],
        features: Vec::new(),
    }
}

fn parse_ogg(data: &[u8]) -> ParsedMedia {
    let mut codec = "ogg".to_string();
    let mut mime = "application/ogg".to_string();
    let mut stream = MediaStreamRecord {
        stream_index: 0,
        stream_type: "audio".to_string(),
        codec: Some(codec.clone()),
        ..MediaStreamRecord::default()
    };
    if let Some(packet) = first_ogg_packet(data) {
        if packet.starts_with(b"OpusHead") && packet.len() >= 19 {
            codec = "opus".to_string();
            mime = "audio/opus".to_string();
            stream.codec = Some(codec.clone());
            stream.channels = Some(packet[9] as i64);
            stream.sample_rate = Some(LittleEndian::read_u32(&packet[12..16]) as i64);
            stream.details_json = Some(
                serde_json::to_string(&json!({
                    "version": packet[8],
                    "pre_skip": LittleEndian::read_u16(&packet[10..12]),
                    "output_gain": LittleEndian::read_i16(&packet[16..18]),
                    "channel_mapping_family": packet[18],
                }))
                .unwrap_or_default(),
            );
        } else if packet.starts_with(&[1])
            && packet.get(1..7) == Some(&b"vorbis"[..])
            && packet.len() >= 30
        {
            codec = "vorbis".to_string();
            mime = "audio/ogg".to_string();
            stream.codec = Some(codec.clone());
            stream.channels = Some(packet[11] as i64);
            stream.sample_rate = Some(LittleEndian::read_u32(&packet[12..16]) as i64);
            stream.bitrate = Some(LittleEndian::read_u32(&packet[16..20]) as i64);
        }
    }
    ParsedMedia {
        media_kind: "audio".to_string(),
        container: "ogg".to_string(),
        detected_format: codec,
        mime: Some(mime),
        scan_status: "ok".to_string(),
        error: None,
        streams: vec![stream],
        features: Vec::new(),
    }
}

fn first_ogg_packet(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 27 || &data[0..4] != b"OggS" {
        return None;
    }
    let segment_count = data[26] as usize;
    if data.len() < 27 + segment_count {
        return None;
    }
    let laces = &data[27..27 + segment_count];
    let size: usize = laces.iter().map(|v| *v as usize).sum();
    let start = 27 + segment_count;
    if start + size > data.len() {
        return None;
    }
    Some(data[start..start + size].to_vec())
}

fn parse_matroska(data: &[u8]) -> ParsedMedia {
    let mut state = EbmlState::default();
    parse_ebml_range(data, 0, data.len(), 0, &mut state);
    let detected = if state.doc_type.as_deref() == Some("webm") {
        "webm"
    } else {
        "mkv"
    };
    let mut streams = Vec::new();
    for (idx, track) in state.tracks.into_iter().enumerate() {
        let stream_type = match track.track_type {
            Some(1) => "video",
            Some(2) => "audio",
            Some(17) => "subtitle",
            _ => "metadata",
        };
        streams.push(MediaStreamRecord {
            stream_index: idx as i64,
            stream_type: stream_type.to_string(),
            codec: track.codec_id.as_deref().map(matroska_codec),
            codec_tag: track.codec_id,
            width: track.pixel_width,
            height: track.pixel_height,
            coded_width: track.pixel_width,
            coded_height: track.pixel_height,
            duration_ms: state.duration_ms,
            sample_rate: track.sample_rate,
            channels: track.channels,
            bit_depth: track.bit_depth,
            details_json: Some(
                serde_json::to_string(&json!({
                    "track_number": track.track_number,
                    "display_width": track.display_width,
                    "display_height": track.display_height,
                    "doc_type": state.doc_type,
                }))
                .unwrap_or_default(),
            ),
            ..MediaStreamRecord::default()
        });
    }
    if streams.is_empty() {
        streams.push(MediaStreamRecord {
            stream_index: 0,
            stream_type: "metadata".to_string(),
            codec: state.doc_type.clone(),
            details_json: Some(
                serde_json::to_string(&json!({
                    "doc_type": state.doc_type,
                    "duration_ms": state.duration_ms,
                    "timecode_scale": state.timecode_scale,
                }))
                .unwrap_or_default(),
            ),
            ..MediaStreamRecord::default()
        });
    }
    let has_video = streams.iter().any(|stream| stream.stream_type == "video");
    let has_audio = streams.iter().any(|stream| stream.stream_type == "audio");
    ParsedMedia {
        media_kind: if has_video {
            "video".to_string()
        } else if has_audio {
            "audio".to_string()
        } else {
            "unknown".to_string()
        },
        container: "matroska".to_string(),
        detected_format: detected.to_string(),
        mime: Some(if detected == "webm" {
            "video/webm".to_string()
        } else {
            "video/x-matroska".to_string()
        }),
        scan_status: if has_video || has_audio {
            "ok".to_string()
        } else {
            "partial".to_string()
        },
        error: (!(has_video || has_audio))
            .then(|| "Matroska tracks not found in probe window".to_string()),
        streams,
        features: Vec::new(),
    }
}

#[derive(Default)]
struct EbmlState {
    doc_type: Option<String>,
    timecode_scale: Option<i64>,
    duration_ms: Option<i64>,
    tracks: Vec<EbmlTrack>,
}

#[derive(Default)]
struct EbmlTrack {
    track_number: Option<i64>,
    track_type: Option<i64>,
    codec_id: Option<String>,
    pixel_width: Option<i64>,
    pixel_height: Option<i64>,
    display_width: Option<i64>,
    display_height: Option<i64>,
    sample_rate: Option<i64>,
    channels: Option<i64>,
    bit_depth: Option<i64>,
}

fn parse_ebml_range(data: &[u8], start: usize, end: usize, depth: usize, state: &mut EbmlState) {
    if depth > 64 {
        return;
    }
    let mut pos = start;
    while let Some(elem) = read_ebml_elem(data, pos, end) {
        match elem.id {
            0x4282 => state.doc_type = Some(ebml_string(&data[elem.start..elem.end])),
            0x2ad7b1 => state.timecode_scale = ebml_uint(&data[elem.start..elem.end]),
            0x4489 => {
                if let Some(duration) = ebml_float(&data[elem.start..elem.end]) {
                    let scale = state.timecode_scale.unwrap_or(1_000_000) as f64;
                    state.duration_ms = Some((duration * scale / 1_000_000.0).round() as i64);
                }
            }
            0xae => {
                let mut track = EbmlTrack::default();
                parse_ebml_track(data, elem.start, elem.end, depth + 1, &mut track);
                state.tracks.push(track);
            }
            id if is_ebml_master(id) => {
                parse_ebml_range(data, elem.start, elem.end, depth + 1, state)
            }
            _ => {}
        }
        if elem.end <= pos {
            break;
        }
        pos = elem.end;
    }
}

fn parse_ebml_track(data: &[u8], start: usize, end: usize, depth: usize, track: &mut EbmlTrack) {
    if depth > 64 {
        return;
    }
    let mut pos = start;
    while let Some(elem) = read_ebml_elem(data, pos, end) {
        match elem.id {
            0xd7 => track.track_number = ebml_uint(&data[elem.start..elem.end]),
            0x83 => track.track_type = ebml_uint(&data[elem.start..elem.end]),
            0x86 => track.codec_id = Some(ebml_string(&data[elem.start..elem.end])),
            0xb0 => track.pixel_width = ebml_uint(&data[elem.start..elem.end]),
            0xba => track.pixel_height = ebml_uint(&data[elem.start..elem.end]),
            0x54b0 => track.display_width = ebml_uint(&data[elem.start..elem.end]),
            0x54ba => track.display_height = ebml_uint(&data[elem.start..elem.end]),
            0xb5 => {
                track.sample_rate =
                    ebml_float(&data[elem.start..elem.end]).map(|v| v.round() as i64)
            }
            0x9f => track.channels = ebml_uint(&data[elem.start..elem.end]),
            0x6264 => track.bit_depth = ebml_uint(&data[elem.start..elem.end]),
            id if is_ebml_master(id) => {
                parse_ebml_track(data, elem.start, elem.end, depth + 1, track)
            }
            _ => {}
        }
        if elem.end <= pos {
            break;
        }
        pos = elem.end;
    }
}

fn is_ebml_master(id: u64) -> bool {
    matches!(
        id,
        0x1a45dfa3 | 0x18538067 | 0x1549a966 | 0x1654ae6b | 0xae | 0xe0 | 0xe1
    )
}

struct EbmlElem {
    id: u64,
    start: usize,
    end: usize,
}

fn read_ebml_elem(data: &[u8], pos: usize, limit: usize) -> Option<EbmlElem> {
    let (id, id_len) = read_ebml_id(data, pos)?;
    let (size, size_len) = read_ebml_size(data, pos + id_len)?;
    let start = pos + id_len + size_len;
    let size = size?;
    let end = start.checked_add(size as usize)?;
    if end > limit || end > data.len() {
        return None;
    }
    Some(EbmlElem { id, start, end })
}

fn read_ebml_id(data: &[u8], pos: usize) -> Option<(u64, usize)> {
    let first = *data.get(pos)?;
    let len = first.leading_zeros() as usize + 1;
    if len == 0 || len > 4 || pos + len > data.len() {
        return None;
    }
    let mut value = 0u64;
    for byte in &data[pos..pos + len] {
        value = (value << 8) | *byte as u64;
    }
    Some((value, len))
}

fn read_ebml_size(data: &[u8], pos: usize) -> Option<(Option<u64>, usize)> {
    let first = *data.get(pos)?;
    let len = first.leading_zeros() as usize + 1;
    if len == 0 || len > 8 || pos + len > data.len() {
        return None;
    }
    let mask = 0xffu8 >> len;
    let mut value = (first & mask) as u64;
    for byte in &data[pos + 1..pos + len] {
        value = (value << 8) | *byte as u64;
    }
    let unknown = value == ((1u64 << (7 * len)) - 1);
    Some(((!unknown).then_some(value), len))
}

fn ebml_uint(data: &[u8]) -> Option<i64> {
    if data.len() > 8 {
        return None;
    }
    let mut value = 0i64;
    for byte in data {
        value = (value << 8) | *byte as i64;
    }
    Some(value)
}

fn ebml_float(data: &[u8]) -> Option<f64> {
    match data.len() {
        4 => Some(f32::from_bits(BigEndian::read_u32(data)) as f64),
        8 => Some(f64::from_bits(BigEndian::read_u64(data))),
        _ => None,
    }
}

fn ebml_string(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .trim_matches(char::from(0))
        .to_string()
}

fn matroska_codec(codec_id: &str) -> String {
    match codec_id {
        "V_VP8" => "vp8",
        "V_VP9" => "vp9",
        "V_AV1" => "av1",
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "h265",
        "A_OPUS" => "opus",
        "A_VORBIS" => "vorbis",
        "A_FLAC" => "flac",
        "A_AAC" | "A_AAC/MPEG4/LC" => "aac",
        other => other,
    }
    .to_string()
}

#[derive(Clone)]
struct BoxHeader {
    typ: [u8; 4],
    size: u64,
    data_start: usize,
    end: usize,
}

fn read_box(data: &[u8], pos: usize, limit: usize) -> Option<BoxHeader> {
    if pos + 8 > limit || pos + 8 > data.len() {
        return None;
    }
    let size32 = BigEndian::read_u32(&data[pos..pos + 4]) as u64;
    let mut typ = [0u8; 4];
    typ.copy_from_slice(&data[pos + 4..pos + 8]);
    let (size, data_start) = if size32 == 1 {
        if pos + 16 > limit || pos + 16 > data.len() {
            return None;
        }
        (BigEndian::read_u64(&data[pos + 8..pos + 16]), pos + 16)
    } else if size32 == 0 {
        ((limit - pos) as u64, pos + 8)
    } else {
        (size32, pos + 8)
    };
    if size < (data_start - pos) as u64 {
        return None;
    }
    let end = pos.checked_add(size as usize)?;
    if end > limit || end > data.len() {
        return None;
    }
    Some(BoxHeader {
        typ,
        size,
        data_start,
        end,
    })
}

fn load_streams(con: &Connection, file_id: i64) -> anyhow::Result<Vec<MediaStreamRecord>> {
    let mut stmt = con.prepare(
        "SELECT stream_index, stream_type, codec, codec_tag, profile, level, width, height,
                coded_width, coded_height, duration_ms, bitrate, fps_num, fps_den,
                sample_rate, channels, bit_depth, pixel_format, chroma_subsampling,
                color_primaries, transfer_characteristics, matrix_coefficients,
                rotation_degrees, details_json
         FROM media_streams WHERE file_id=?1 ORDER BY stream_index",
    )?;
    let rows = stmt.query_map(params![file_id], |row| {
        Ok(MediaStreamRecord {
            stream_index: row.get(0)?,
            stream_type: row.get(1)?,
            codec: row.get(2)?,
            codec_tag: row.get(3)?,
            profile: row.get(4)?,
            level: row.get(5)?,
            width: row.get(6)?,
            height: row.get(7)?,
            coded_width: row.get(8)?,
            coded_height: row.get(9)?,
            duration_ms: row.get(10)?,
            bitrate: row.get(11)?,
            fps_num: row.get(12)?,
            fps_den: row.get(13)?,
            sample_rate: row.get(14)?,
            channels: row.get(15)?,
            bit_depth: row.get(16)?,
            pixel_format: row.get(17)?,
            chroma_subsampling: row.get(18)?,
            color_primaries: row.get(19)?,
            transfer_characteristics: row.get(20)?,
            matrix_coefficients: row.get(21)?,
            rotation_degrees: row.get(22)?,
            details_json: row.get(23)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn load_features(con: &Connection, file_id: i64) -> anyhow::Result<Vec<MediaCodecFeatureRecord>> {
    let mut stmt = con.prepare(
        "SELECT stream_index, standard, coding_mode, transform, prediction, entropy_coding,
                progressive, lossless, near_lossless, alpha, animation, icc_profile, exif,
                xmp, orientation, confidence, features_json
         FROM media_codec_features WHERE file_id=?1 ORDER BY stream_index",
    )?;
    let rows = stmt.query_map(params![file_id], |row| {
        Ok(MediaCodecFeatureRecord {
            stream_index: row.get(0)?,
            standard: row.get(1)?,
            coding_mode: row.get(2)?,
            transform: row.get(3)?,
            prediction: row.get(4)?,
            entropy_coding: row.get(5)?,
            progressive: row.get(6)?,
            lossless: row.get(7)?,
            near_lossless: row.get(8)?,
            alpha: row.get(9)?,
            animation: row.get(10)?,
            icc_profile: row.get(11)?,
            exif: row.get(12)?,
            xmp: row.get(13)?,
            orientation: row.get(14)?,
            confidence: row.get(15)?,
            features_json: row.get(16)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn media_file_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, MediaFileRecord)> {
    Ok((
        row.get(0)?,
        MediaFileRecord {
            path: row.get(1)?,
            source_root: row.get(2)?,
            relative_path: row.get(3)?,
            file_name: row.get(4)?,
            extension: row.get(5)?,
            file_size: row.get(6)?,
            mtime_unix: row.get(7)?,
            media_kind: row.get(8)?,
            container: row.get(9)?,
            detected_format: row.get(10)?,
            mime: row.get(11)?,
            hash_mode: row.get(12)?,
            sha256_hex: row.get(13)?,
            probe_version: row.get(14)?,
            scan_status: row.get(15)?,
            error: row.get(16)?,
            scanned_at_unix: row.get(17)?,
        },
    ))
}

fn grouped_counts(con: &Connection, column: &str) -> anyhow::Result<BTreeMap<String, i64>> {
    let sql = format!(
        "SELECT {column}, count(*) FROM media_files GROUP BY {column} ORDER BY count(*) DESC, {column}"
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?
                .unwrap_or_else(|| "unknown".to_string()),
            row.get::<_, i64>(1)?,
        ))
    })?;
    let mut counts = BTreeMap::new();
    for row in rows {
        let (key, value) = row?;
        counts.insert(key, value);
    }
    Ok(counts)
}

fn count_i64(con: &Connection, sql: &str) -> anyhow::Result<i64> {
    con.query_row(sql, [], |row| row.get(0)).map_err(Into::into)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn json_limited(value: &Value) -> (String, bool) {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    if text.len() <= MAX_DETAILS_JSON_BYTES {
        return (text, false);
    }
    let prefix = text
        .chars()
        .take(MAX_DETAILS_JSON_BYTES.min(4096))
        .collect::<String>();
    (
        serde_json::to_string(&json!({
            "truncated": true,
            "original_len": text.len(),
            "prefix": prefix,
        }))
        .unwrap_or_else(|_| "{\"truncated\":true}".to_string()),
        true,
    )
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn modified_unix(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
}

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
}

fn format_from_mime(mime: &str) -> Option<String> {
    mime.rsplit('/').next().map(|value| value.to_string())
}

fn is_jpeg(data: &[u8]) -> bool {
    data.len() >= 3 && data[0..3] == [0xff, 0xd8, 0xff]
}

fn is_png(data: &[u8]) -> bool {
    data.starts_with(b"\x89PNG\r\n\x1a\n")
}

fn is_gif(data: &[u8]) -> bool {
    data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")
}

fn is_webp(data: &[u8]) -> bool {
    data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP"
}

fn is_jp2(data: &[u8]) -> bool {
    data.starts_with(b"\0\0\0\x0cjP  \r\n\x87\n")
}

fn is_j2k(data: &[u8]) -> bool {
    data.starts_with(&[0xff, 0x4f, 0xff, 0x51])
}

fn is_jxl(data: &[u8]) -> bool {
    data.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") || data.starts_with(&[0xff, 0x0a])
}

fn is_iso_bmff(data: &[u8]) -> bool {
    read_box(data, 0, data.len())
        .map(|header| header.typ == *b"ftyp" || header.typ == *b"moov")
        .unwrap_or(false)
}

fn has_complete_top_level_box(data: &[u8], typ: &[u8; 4]) -> bool {
    let mut pos = 0usize;
    while let Some(header) = read_box(data, pos, data.len()) {
        if header.typ == *typ {
            return true;
        }
        if header.end <= pos || header.end >= data.len() {
            break;
        }
        pos = header.end;
    }
    false
}

fn is_riff(data: &[u8]) -> bool {
    data.len() >= 12 && &data[0..4] == b"RIFF"
}

fn is_mp3(data: &[u8]) -> bool {
    find_mp3_probe(data).is_some()
}

fn is_flac(data: &[u8]) -> bool {
    data.starts_with(b"fLaC")
}

fn is_ogg(data: &[u8]) -> bool {
    data.starts_with(b"OggS")
}

fn is_matroska(data: &[u8]) -> bool {
    data.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
}

fn is_zero_filled_probe(data: &[u8]) -> bool {
    !data.is_empty() && data.iter().all(|byte| *byte == 0)
}

fn silk_header_offset(data: &[u8]) -> Option<usize> {
    if data.starts_with(b"#!SILK_V3") {
        Some(0)
    } else if data.len() > 1 && data[0] == 0x02 && data[1..].starts_with(b"#!SILK_V3") {
        Some(1)
    } else {
        None
    }
}

fn is_silk(data: &[u8]) -> bool {
    silk_header_offset(data).is_some()
}

fn is_pdf(data: &[u8]) -> bool {
    data.starts_with(b"%PDF-")
}

fn is_svg(data: &[u8]) -> bool {
    let trimmed = trim_xml_probe_start(data);
    trimmed.starts_with(b"<svg")
        || trimmed
            .windows(4)
            .take(512)
            .any(|window| window.eq_ignore_ascii_case(b"<svg"))
}

fn is_xml_document(data: &[u8], file: &MediaFileRecord) -> bool {
    let trimmed = trim_xml_probe_start(data);
    file.extension.as_deref() == Some("xml")
        || file.mime.as_deref() == Some("text/xml")
        || trimmed.starts_with(b"<?xml")
}

fn trim_xml_probe_start(mut data: &[u8]) -> &[u8] {
    if data.starts_with(&[0xef, 0xbb, 0xbf]) {
        data = &data[3..];
    }
    while let Some((first, rest)) = data.split_first() {
        if first.is_ascii_whitespace() {
            data = rest;
        } else {
            break;
        }
    }
    data
}

fn u16be(data: &[u8], off: usize) -> Option<u16> {
    (off + 2 <= data.len()).then(|| BigEndian::read_u16(&data[off..off + 2]))
}

fn u16le(data: &[u8], off: usize) -> Option<u16> {
    (off + 2 <= data.len()).then(|| LittleEndian::read_u16(&data[off..off + 2]))
}

fn read_u24le(data: &[u8]) -> u32 {
    data[0] as u32 | ((data[1] as u32) << 8) | ((data[2] as u32) << 16)
}

fn read_u24be(data: &[u8]) -> u32 {
    ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | data[2] as u32
}

fn fourcc(data: &[u8]) -> String {
    String::from_utf8_lossy(data).to_string()
}

fn synchsafe_u32(data: &[u8]) -> u32 {
    ((data[0] as u32 & 0x7f) << 21)
        | ((data[1] as u32 & 0x7f) << 14)
        | ((data[2] as u32 & 0x7f) << 7)
        | (data[3] as u32 & 0x7f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "qq-media-info-{name}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn media_info_jpeg_marker_classification() {
        let baseline = jpeg_fixture(0xc0);
        let progressive = jpeg_fixture(0xc2);
        let lossless = jpeg_fixture(0xc3);
        let jls = jpeg_fixture(0xf7);
        assert_eq!(
            parse_jpeg(&baseline).features[0].coding_mode.as_deref(),
            Some("baseline_or_sequential")
        );
        assert_eq!(
            parse_jpeg(&progressive).features[0].progressive.as_deref(),
            Some("true")
        );
        assert_eq!(parse_jpeg(&lossless).features[0].lossless, Some(1));
        assert_eq!(
            parse_jpeg(&jls).features[0].standard.as_deref(),
            Some("jpeg-ls")
        );
    }

    fn jpeg_fixture(sof: u8) -> Vec<u8> {
        vec![
            0xff, 0xd8, 0xff, 0xe1, 0x00, 0x08, b'E', b'x', b'i', b'f', 0, 0, 0xff, sof, 0x00,
            0x11, 8, 0, 16, 0, 32, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1, 0xff, 0xda, 0x00, 0x08,
            1, 1, 0, 0, 63, 0,
        ]
    }

    #[test]
    fn media_info_png_detects_alpha_icc_srgb_apng() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png_chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 3, 8, 6, 0, 0, 0]);
        png_chunk(&mut png, b"iCCP", b"profile\0x");
        png_chunk(&mut png, b"sRGB", &[0]);
        png_chunk(&mut png, b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]);
        png_chunk(&mut png, b"IEND", &[]);
        let parsed = parse_png(&png);
        assert_eq!(parsed.media_kind, "animated_image");
        assert_eq!(parsed.streams[0].width, Some(2));
        assert_eq!(parsed.features[0].alpha, Some(1));
        assert_eq!(parsed.features[0].icc_profile, Some(1));
        assert_eq!(parsed.features[0].animation, Some(1));
    }

    fn png_chunk(out: &mut Vec<u8>, typ: &[u8; 4], payload: &[u8]) {
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(typ);
        out.extend_from_slice(payload);
        out.extend_from_slice(&0u32.to_be_bytes());
    }

    #[test]
    fn media_info_webp_vp8x_flags() {
        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();
        let mut vp8x = vec![0x12, 0, 0, 0];
        vp8x.extend_from_slice(&[9, 0, 0, 19, 0, 0]);
        webp_chunk(&mut webp, b"VP8X", &vp8x);
        let size = (webp.len() - 8) as u32;
        webp[4..8].copy_from_slice(&size.to_le_bytes());
        let parsed = parse_webp(&webp);
        assert_eq!(parsed.media_kind, "animated_image");
        assert_eq!(parsed.streams[0].width, Some(10));
        assert_eq!(parsed.streams[0].height, Some(20));
        assert_eq!(parsed.features[0].alpha, Some(1));
    }

    #[test]
    fn media_info_webp_vp8_and_vp8l_headers() {
        let mut vp8 = b"RIFF\0\0\0\0WEBP".to_vec();
        let mut vp8_payload = vec![0, 0, 0, 0x9d, 0x01, 0x2a];
        vp8_payload.extend_from_slice(&(30u16).to_le_bytes());
        vp8_payload.extend_from_slice(&(40u16).to_le_bytes());
        webp_chunk(&mut vp8, b"VP8 ", &vp8_payload);
        let size = (vp8.len() - 8) as u32;
        vp8[4..8].copy_from_slice(&size.to_le_bytes());
        let parsed = parse_webp(&vp8);
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("webp-vp8"));
        assert_eq!(parsed.streams[0].width, Some(30));
        assert_eq!(parsed.streams[0].height, Some(40));
        assert_eq!(parsed.features[0].lossless, Some(0));

        let mut vp8l = b"RIFF\0\0\0\0WEBP".to_vec();
        webp_chunk(&mut vp8l, b"VP8L", &[0x2f, 9, 0xc0, 4, 0x10]);
        let size = (vp8l.len() - 8) as u32;
        vp8l[4..8].copy_from_slice(&size.to_le_bytes());
        let parsed = parse_webp(&vp8l);
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("webp-vp8l"));
        assert_eq!(parsed.streams[0].width, Some(10));
        assert_eq!(parsed.streams[0].height, Some(20));
        assert_eq!(parsed.features[0].lossless, Some(1));
    }

    fn webp_chunk(out: &mut Vec<u8>, typ: &[u8; 4], payload: &[u8]) {
        out.extend_from_slice(typ);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            out.push(0);
        }
    }

    #[test]
    fn media_info_gif_counts_frames() {
        let mut gif = b"GIF89a\x01\0\x01\0\0\0\0".to_vec();
        gif.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 1, 0, 0]);
        gif.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 1, 0, 0, 0x3b]);
        let parsed = parse_gif(&gif);
        assert_eq!(parsed.media_kind, "animated_image");
        assert_eq!(parsed.features[0].animation, Some(1));
    }

    #[test]
    fn media_info_mp4_synthetic_track() {
        let mp4 = synthetic_mp4();
        let parsed = parse_iso_bmff(&mp4, mp4.len() as u64);
        assert_eq!(parsed.media_kind, "video");
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("h264"));
        assert_eq!(parsed.streams[0].width, Some(320));
        assert_eq!(parsed.streams[0].height, Some(240));
        assert_eq!(parsed.streams[0].fps_num, Some(1000));
        assert_eq!(parsed.streams[0].fps_den, Some(40));
    }

    #[test]
    fn media_info_mp4_tail_moov_finds_tracks() {
        let mp4 = synthetic_mp4();
        let ftyp = read_box(&mp4, 0, mp4.len()).unwrap();
        let head = &mp4[..ftyp.end];
        let tail = TailProbe {
            offset: 10_000,
            data: mp4[ftyp.end..].to_vec(),
        };
        let parsed =
            parse_iso_bmff_with_tail(head, Some(&tail), tail.offset + tail.data.len() as u64);
        assert_eq!(parsed.media_kind, "video");
        assert_eq!(parsed.scan_status, "ok");
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("h264"));
        assert!(parsed.streams[0]
            .details_json
            .as_deref()
            .unwrap()
            .contains("sample_entries"));
    }

    #[test]
    fn media_info_mp4_esds_refines_audio_codec() {
        let aac = parse_sample_entry(
            "mp4a",
            &mp4_audio_sample_entry_with_esds(0x40, Some(&aac_lc_audio_specific_config(3, 2))),
        );
        assert_eq!(aac.codec, "aac");
        assert_eq!(aac.profile.as_deref(), Some("aac-lc"));
        assert_eq!(aac.sample_rate, Some(48_000));
        assert_eq!(aac.channels, Some(2));
        assert_eq!(aac.bitrate, Some(96_000));

        let mp3 = parse_sample_entry("mp4a", &mp4_audio_sample_entry_with_esds(0x6b, None));
        assert_eq!(mp3.codec, "mp3");
        assert_eq!(mp3.bitrate, Some(96_000));
    }

    #[test]
    fn media_info_audio_headers() {
        let mp3 = mp3_fixture_with_id3_xing();
        assert!(is_mp3(&mp3));
        let parsed = parse_mp3(&mp3, mp3.len() as u64);
        assert_eq!(parsed.scan_status, "ok");
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("mp3"));
        assert_eq!(parsed.streams[0].sample_rate, Some(44100));
        assert_eq!(parsed.streams[0].channels, Some(2));
        let details: Value =
            serde_json::from_str(parsed.streams[0].details_json.as_deref().unwrap()).unwrap();
        assert_eq!(details["id3v2"]["version"], 4);
        assert_eq!(details["vbr"]["marker"], "xing");
        assert_eq!(details["frame_consistency"]["checked_frames"], 4);

        let mut flac = b"fLaC".to_vec();
        let mut streaminfo = vec![0u8; 34];
        streaminfo[0..2].copy_from_slice(&16u16.to_be_bytes());
        streaminfo[2..4].copy_from_slice(&16u16.to_be_bytes());
        let packed = ((44100u64) << 44) | ((1u64) << 41) | ((15u64) << 36) | 44100u64;
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
        flac.push(0x80);
        flac.extend_from_slice(&[0, 0, 34]);
        flac.extend_from_slice(&streaminfo);
        let parsed = parse_flac(&flac);
        assert_eq!(parsed.streams[0].sample_rate, Some(44100));
        assert_eq!(parsed.streams[0].channels, Some(2));

        let mut ogg = b"OggS\0\x02".to_vec();
        ogg.extend_from_slice(&[0; 20]);
        ogg.push(1);
        ogg.push(19);
        ogg.extend_from_slice(b"OpusHead");
        ogg.extend_from_slice(&[1, 2]);
        ogg.extend_from_slice(&312u16.to_le_bytes());
        ogg.extend_from_slice(&48000u32.to_le_bytes());
        ogg.extend_from_slice(&0i16.to_le_bytes());
        ogg.push(0);
        let parsed = parse_ogg(&ogg);
        assert_eq!(parsed.detected_format, "opus");
        assert_eq!(parsed.streams[0].channels, Some(2));
    }

    #[test]
    fn media_info_mp3_rejects_single_sync_false_positive() {
        let mut blob = vec![0u8; 4096];
        blob[128..132].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        assert!(!is_mp3(&blob));

        let mut id3_only = b"ID3\x04\0\0".to_vec();
        id3_only.extend_from_slice(&id3_synchsafe(32));
        id3_only.extend_from_slice(&[0u8; 32]);
        assert!(!is_mp3(&id3_only));
    }

    #[test]
    fn media_info_matroska_minimal_track() {
        let mkv = synthetic_matroska();
        let parsed = parse_matroska(&mkv);
        assert_eq!(parsed.detected_format, "webm");
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("vp9"));
        assert_eq!(parsed.streams[0].width, Some(640));
        assert_eq!(parsed.streams[0].height, Some(360));
    }

    #[test]
    fn media_info_jpeg2000_jxl_avif_and_wav_headers() {
        let parsed = parse_jpeg2000(&j2k_codestream_fixture());
        assert_eq!(parsed.detected_format, "j2k");
        assert_eq!(parsed.streams[0].width, Some(800));
        assert_eq!(parsed.streams[0].height, Some(600));
        assert!(parsed.features[0]
            .features_json
            .as_deref()
            .unwrap()
            .contains("LRCP"));

        let parsed = parse_jpeg_xl(b"\0\0\0\x0cJXL \r\n\x87\n");
        assert_eq!(parsed.detected_format, "jxl");
        assert_eq!(parsed.features[0].confidence.as_deref(), Some("header"));

        let avif = mp4_box(
            b"ftyp",
            &[b"avif".as_slice(), &0u32.to_be_bytes(), b"avif".as_slice()].concat(),
        );
        let parsed = parse_iso_bmff(&avif, avif.len() as u64);
        assert_eq!(parsed.detected_format, "avif");
        assert_eq!(parsed.media_kind, "image");

        let wav = wav_fixture();
        let parsed = parse_riff(&wav, wav.len() as u64);
        assert_eq!(parsed.detected_format, "wav");
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("pcm"));
        assert_eq!(parsed.streams[0].channels, Some(2));
        assert_eq!(parsed.streams[0].sample_rate, Some(44100));
        assert_eq!(parsed.streams[0].bit_depth, Some(16));
    }

    #[test]
    fn media_info_mp4_audio_track_rotation_and_duration() {
        let mp4 = synthetic_mp4_video_audio_rotated();
        let parsed = parse_iso_bmff(&mp4, mp4.len() as u64);
        assert_eq!(parsed.media_kind, "video");
        assert_eq!(parsed.streams.len(), 2);
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("h264"));
        assert_eq!(parsed.streams[0].rotation_degrees, Some(90));
        assert_eq!(parsed.streams[0].duration_ms, Some(5000));
        assert_eq!(parsed.streams[1].codec.as_deref(), Some("aac"));
        assert_eq!(parsed.streams[1].sample_rate, Some(48000));
        assert_eq!(parsed.streams[1].channels, Some(2));
    }

    #[test]
    fn media_info_unknown_is_unsupported() {
        let root = temp_root("unknown");
        let path = root.join("blob.bin");
        write_file(&path, b"not media");
        let report = inspect_file(&path, HashMode::None).unwrap();
        assert_eq!(report.file.scan_status, "unsupported");
        assert_eq!(report.file.media_kind, "unknown");
    }

    #[test]
    fn media_info_corrupt_file_is_partial_without_panic() {
        let root = temp_root("corrupt");
        let path = root.join("broken.png");
        write_file(&path, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0");
        let report = inspect_file(&path, HashMode::None).unwrap();
        assert_eq!(report.file.detected_format, "png");
        assert_eq!(report.file.scan_status, "partial");
    }

    #[test]
    fn media_info_large_image_probe_truncation_is_ok_header_only() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png_chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 3, 8, 6, 0, 0, 0]);
        png.extend_from_slice(&100_000u32.to_be_bytes());
        png.extend_from_slice(b"IDAT");
        let parsed = parse_png_with_file_size(&png, png.len() as u64 + 100_000);
        assert_eq!(parsed.scan_status, "ok");
        assert_eq!(parsed.error, None);
        assert!(parsed.streams[0]
            .details_json
            .as_deref()
            .unwrap()
            .contains("\"probe_truncated\":true"));
    }

    #[test]
    fn media_info_silk_svg_xml_and_zero_filled_classification() {
        let root = temp_root("classify");

        let silk = root.join("voice.amr");
        write_file(&silk, b"\x02#!SILK_V3\x22\0abc");
        let report = inspect_file(&silk, HashMode::None).unwrap();
        assert_eq!(report.file.media_kind, "audio");
        assert_eq!(report.file.container, "silk");
        assert_eq!(report.streams[0].codec.as_deref(), Some("silk"));

        let svg = root.join("icon.svg");
        write_file(
            &svg,
            br#"<svg width="24" height="32" viewBox="0 0 24 32"></svg>"#,
        );
        let report = inspect_file(&svg, HashMode::None).unwrap();
        assert_eq!(report.file.media_kind, "image_vector");
        assert_eq!(report.file.container, "svg");
        assert_eq!(report.file.mime.as_deref(), Some("image/svg+xml"));
        assert_eq!(report.streams[0].width, Some(24));
        assert_eq!(report.streams[0].height, Some(32));

        let xml = root.join("config.xml");
        write_file(&xml, br#"<?xml version="1.0"?><config/>"#);
        assert!(should_consider_path(&xml));
        let report = inspect_file(&xml, HashMode::None).unwrap();
        assert_eq!(report.file.media_kind, "document");
        assert_eq!(report.file.detected_format, "xml");
        assert_eq!(report.file.scan_status, "ok");

        let zeros = root.join("placeholder.jpg");
        write_file(&zeros, &[0u8; 4096]);
        let report = inspect_file(&zeros, HashMode::None).unwrap();
        assert_eq!(report.file.scan_status, "unsupported");
        assert_eq!(
            report.file.error.as_deref(),
            Some("zero-filled placeholder")
        );
    }

    #[test]
    fn media_info_build_status_export_and_hash() {
        let root = temp_root("build");
        let account = "10000";
        let asset = root.join("assets");
        fs::create_dir_all(&asset).unwrap();
        write_file(&asset.join("a.jpg"), &jpeg_fixture(0xc0));
        write_file(&asset.join("b.bin"), b"unknown");
        let sqlite = root.join("media.sqlite");
        let report = build_index(BuildOptions {
            root: root.clone(),
            account: account.to_string(),
            asset_roots: vec![asset.clone()],
            sqlite_path: Some(sqlite.clone()),
            max_files: 100,
            workers: 2,
            force: false,
            hash_mode: HashMode::None,
        })
        .unwrap();
        assert_eq!(report.indexed_files, 2);
        assert_eq!(report.unsupported_files, 1);
        let rerun = build_index(BuildOptions {
            root: root.clone(),
            account: account.to_string(),
            asset_roots: vec![asset.clone()],
            sqlite_path: Some(sqlite.clone()),
            max_files: 100,
            workers: 1,
            force: false,
            hash_mode: HashMode::None,
        })
        .unwrap();
        assert_eq!(rerun.unchanged_files, 2);
        let status = status(StatusOptions {
            root: root.clone(),
            account: account.to_string(),
            sqlite_path: Some(sqlite.clone()),
        })
        .unwrap();
        assert_eq!(status.total_files, 2);
        let con = Connection::open(&sqlite).unwrap();
        let hashes: i64 = con
            .query_row(
                "SELECT count(*) FROM media_files WHERE sha256_hex IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hashes, 0);

        let hash_report = build_index(BuildOptions {
            root: root.clone(),
            account: account.to_string(),
            asset_roots: vec![asset.clone()],
            sqlite_path: Some(sqlite.clone()),
            max_files: 100,
            workers: 1,
            force: true,
            hash_mode: HashMode::Full,
        })
        .unwrap();
        assert_eq!(hash_report.indexed_files, 2);
        let con = Connection::open(&sqlite).unwrap();
        let hashes: i64 = con
            .query_row(
                "SELECT count(*) FROM media_files WHERE sha256_hex IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hashes, 2);

        let jsonl = root.join("export.jsonl");
        let exported = export(ExportOptions {
            sqlite_path: sqlite.clone(),
            format: ExportFormat::Jsonl,
            out: jsonl.clone(),
        })
        .unwrap();
        assert_eq!(exported.rows, 2);
        assert!(fs::read_to_string(jsonl).unwrap().contains("\"streams\""));
        let csv = root.join("export.csv");
        let exported = export(ExportOptions {
            sqlite_path: sqlite,
            format: ExportFormat::Csv,
            out: csv.clone(),
        })
        .unwrap();
        assert_eq!(exported.rows, 2);
        assert!(fs::read_to_string(csv).unwrap().contains("media_kind"));
    }

    #[test]
    fn media_info_repair_reprobes_suspicious_mp3_rows() {
        let root = temp_root("repair");
        let account = "10000";
        let asset = root.join("assets");
        fs::create_dir_all(&asset).unwrap();
        let suspicious = asset.join("not-mp3.bin");
        let mut blob = vec![0u8; 4096];
        blob[128..132].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        write_file(&suspicious, &blob);
        let sqlite = root.join("media.sqlite");
        {
            let con = Connection::open(&sqlite).unwrap();
            configure_connection(&con).unwrap();
            init_schema(&con).unwrap();
            con.execute(
                "INSERT INTO media_files (
                    path, file_name, extension, file_size, mtime_unix, media_kind, container,
                    detected_format, mime, hash_mode, probe_version, scan_status, scanned_at_unix
                 ) VALUES (?1, ?2, 'bin', ?3, 1, 'audio', 'mp3', 'mp3', 'audio/mpeg',
                           'none', 'old', 'ok', 1)",
                params![path_string(&suspicious), "not-mp3.bin", blob.len() as i64],
            )
            .unwrap();
            let file_id = con.last_insert_rowid();
            con.execute(
                "INSERT INTO media_streams (file_id, stream_index, stream_type, codec, profile)
                 VALUES (?1, 0, 'audio', 'mp3', 'layer1')",
                params![file_id],
            )
            .unwrap();
        }

        let report = repair_index(RepairOptions {
            root: root.clone(),
            account: account.to_string(),
            sqlite_path: Some(sqlite.clone()),
            max_files: 0,
            workers: 1,
            hash_mode: HashMode::None,
        })
        .unwrap();
        assert_eq!(report.candidate_rows, 1);
        assert_eq!(report.changed_files, 1);
        assert_eq!(report.unsupported_files, 1);

        let con = Connection::open(&sqlite).unwrap();
        let row: (String, String, String) = con
            .query_row(
                "SELECT media_kind, container, scan_status FROM media_files WHERE path=?1",
                params![path_string(&suspicious)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "unknown".to_string(),
                "unknown".to_string(),
                "unsupported".to_string()
            )
        );
        let streams: i64 = con
            .query_row("SELECT count(*) FROM media_streams", [], |row| row.get(0))
            .unwrap();
        assert_eq!(streams, 0);
    }

    fn synthetic_mp4() -> Vec<u8> {
        let mut ftyp_payload = Vec::new();
        ftyp_payload.extend_from_slice(b"isom");
        ftyp_payload.extend_from_slice(&0u32.to_be_bytes());
        ftyp_payload.extend_from_slice(b"isom");
        ftyp_payload.extend_from_slice(b"mp42");
        let ftyp = mp4_box(b"ftyp", &ftyp_payload);
        let mvhd_payload = {
            let mut p = vec![0u8; 100];
            p[12..16].copy_from_slice(&1000u32.to_be_bytes());
            p[16..20].copy_from_slice(&5000u32.to_be_bytes());
            p
        };
        let tkhd_payload = {
            let mut p = vec![0u8; 88];
            p[44..48].copy_from_slice(&0x0001_0000u32.to_be_bytes());
            p[60..64].copy_from_slice(&0x0001_0000u32.to_be_bytes());
            p[80..84].copy_from_slice(&(320u32 << 16).to_be_bytes());
            p[84..88].copy_from_slice(&(240u32 << 16).to_be_bytes());
            p
        };
        let mdhd_payload = {
            let mut p = vec![0u8; 20];
            p[12..16].copy_from_slice(&1000u32.to_be_bytes());
            p[16..20].copy_from_slice(&5000u32.to_be_bytes());
            p
        };
        let mut hdlr_payload = vec![0u8; 12];
        hdlr_payload[8..12].copy_from_slice(b"vide");
        let stsd_payload = {
            let mut avcc = vec![1, 66, 0, 31];
            let avcc_box = mp4_box(b"avcC", &avcc);
            let mut avc1 = vec![0u8; 86];
            avc1[32..34].copy_from_slice(&320u16.to_be_bytes());
            avc1[34..36].copy_from_slice(&240u16.to_be_bytes());
            avc1.extend_from_slice(&avcc_box);
            let avc1_box = mp4_box(b"avc1", &avc1);
            let mut p = vec![0u8; 8];
            p[4..8].copy_from_slice(&1u32.to_be_bytes());
            p.extend_from_slice(&avc1_box);
            avcc.clear();
            p
        };
        let stsd = mp4_box(b"stsd", &stsd_payload);
        let stbl = mp4_box(b"stbl", &[stsd, mp4_stts(125, 40), mp4_stsz(125)].concat());
        let minf = mp4_box(b"minf", &stbl);
        let mdia = mp4_box(
            b"mdia",
            &[
                mp4_box(b"mdhd", &mdhd_payload),
                mp4_box(b"hdlr", &hdlr_payload),
                minf,
            ]
            .concat(),
        );
        let trak = mp4_box(b"trak", &[mp4_box(b"tkhd", &tkhd_payload), mdia].concat());
        let moov = mp4_box(b"moov", &[mp4_box(b"mvhd", &mvhd_payload), trak].concat());
        [ftyp, moov].concat()
    }

    fn synthetic_mp4_video_audio_rotated() -> Vec<u8> {
        let mut ftyp_payload = Vec::new();
        ftyp_payload.extend_from_slice(b"isom");
        ftyp_payload.extend_from_slice(&0u32.to_be_bytes());
        ftyp_payload.extend_from_slice(b"isom");
        ftyp_payload.extend_from_slice(b"mp42");
        let ftyp = mp4_box(b"ftyp", &ftyp_payload);
        let mvhd_payload = {
            let mut p = vec![0u8; 100];
            p[12..16].copy_from_slice(&1000u32.to_be_bytes());
            p[16..20].copy_from_slice(&5000u32.to_be_bytes());
            p
        };
        let video_trak = mp4_track(
            b"vide",
            Some((320, 240)),
            Some(90),
            1000,
            5000,
            mp4_video_sample_entry(),
        );
        let audio_trak = mp4_track(
            b"soun",
            None,
            None,
            48_000,
            240_000,
            mp4_audio_sample_entry(),
        );
        let moov = mp4_box(
            b"moov",
            &[mp4_box(b"mvhd", &mvhd_payload), video_trak, audio_trak].concat(),
        );
        [ftyp, moov].concat()
    }

    fn mp4_track(
        handler: &[u8; 4],
        dimensions: Option<(u16, u16)>,
        rotation: Option<i64>,
        timescale: u32,
        duration: u32,
        sample_entry: Vec<u8>,
    ) -> Vec<u8> {
        let tkhd_payload = {
            let mut p = vec![0u8; 88];
            let (a, b, c, d): (i32, i32, i32, i32) = match rotation {
                Some(90) => (0, 0x0001_0000, -0x0001_0000, 0),
                Some(180) => (-0x0001_0000, 0, 0, -0x0001_0000),
                Some(270) => (0, -0x0001_0000, 0x0001_0000, 0),
                _ => (0x0001_0000, 0, 0, 0x0001_0000),
            };
            p[44..48].copy_from_slice(&a.to_be_bytes());
            p[48..52].copy_from_slice(&b.to_be_bytes());
            p[56..60].copy_from_slice(&c.to_be_bytes());
            p[60..64].copy_from_slice(&d.to_be_bytes());
            if let Some((width, height)) = dimensions {
                p[80..84].copy_from_slice(&((width as u32) << 16).to_be_bytes());
                p[84..88].copy_from_slice(&((height as u32) << 16).to_be_bytes());
            }
            p
        };
        let mdhd_payload = {
            let mut p = vec![0u8; 20];
            p[12..16].copy_from_slice(&timescale.to_be_bytes());
            p[16..20].copy_from_slice(&duration.to_be_bytes());
            p
        };
        let mut hdlr_payload = vec![0u8; 12];
        hdlr_payload[8..12].copy_from_slice(handler);
        let mut stsd_payload = vec![0u8; 8];
        stsd_payload[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsd_payload.extend_from_slice(&sample_entry);
        let stsd = mp4_box(b"stsd", &stsd_payload);
        let sample_delta = if handler == b"vide" {
            timescale / 25
        } else {
            1024
        };
        let stbl = mp4_box(
            b"stbl",
            &[stsd, mp4_stts(125, sample_delta.max(1)), mp4_stsz(125)].concat(),
        );
        let minf = mp4_box(b"minf", &stbl);
        let mdia = mp4_box(
            b"mdia",
            &[
                mp4_box(b"mdhd", &mdhd_payload),
                mp4_box(b"hdlr", &hdlr_payload),
                minf,
            ]
            .concat(),
        );
        mp4_box(b"trak", &[mp4_box(b"tkhd", &tkhd_payload), mdia].concat())
    }

    fn mp4_video_sample_entry() -> Vec<u8> {
        let avcc_box = mp4_box(b"avcC", &[1, 66, 0, 31]);
        let mut avc1 = vec![0u8; 78];
        avc1[24..26].copy_from_slice(&320u16.to_be_bytes());
        avc1[26..28].copy_from_slice(&240u16.to_be_bytes());
        avc1.extend_from_slice(&avcc_box);
        mp4_box(b"avc1", &avc1)
    }

    fn mp4_audio_sample_entry() -> Vec<u8> {
        let mut mp4a = vec![0u8; 28];
        mp4a[16..18].copy_from_slice(&2u16.to_be_bytes());
        mp4a[18..20].copy_from_slice(&16u16.to_be_bytes());
        mp4a[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes());
        mp4_box(b"mp4a", &mp4a)
    }

    fn mp4_audio_sample_entry_with_esds(object_type: u8, asc: Option<&[u8]>) -> Vec<u8> {
        let mut decoder_config = vec![object_type, 0x15, 0, 0, 0];
        decoder_config.extend_from_slice(&128_000u32.to_be_bytes());
        decoder_config.extend_from_slice(&96_000u32.to_be_bytes());
        if let Some(asc) = asc {
            decoder_config.extend_from_slice(&mp4_descriptor(0x05, asc));
        }
        let es_descriptor = {
            let mut payload = vec![0, 1, 0];
            payload.extend_from_slice(&mp4_descriptor(0x04, &decoder_config));
            mp4_descriptor(0x03, &payload)
        };
        let mut esds_payload = vec![0u8; 4];
        esds_payload.extend_from_slice(&es_descriptor);
        let esds = mp4_box(b"esds", &esds_payload);

        let mut mp4a = vec![0u8; 28];
        mp4a[16..18].copy_from_slice(&2u16.to_be_bytes());
        mp4a[18..20].copy_from_slice(&16u16.to_be_bytes());
        mp4a[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes());
        mp4a.extend_from_slice(&esds);
        mp4_box(b"mp4a", &mp4a)
    }

    fn mp4_descriptor(tag: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128);
        let mut out = vec![tag, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    fn aac_lc_audio_specific_config(freq_index: u16, channels: u16) -> [u8; 2] {
        let packed = (2u16 << 11) | (freq_index << 7) | (channels << 3);
        packed.to_be_bytes()
    }

    fn mp4_stts(sample_count: u32, sample_delta: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 16];
        payload[4..8].copy_from_slice(&1u32.to_be_bytes());
        payload[8..12].copy_from_slice(&sample_count.to_be_bytes());
        payload[12..16].copy_from_slice(&sample_delta.to_be_bytes());
        mp4_box(b"stts", &payload)
    }

    fn mp4_stsz(sample_count: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 12];
        payload[8..12].copy_from_slice(&sample_count.to_be_bytes());
        mp4_box(b"stsz", &payload)
    }

    fn mp4_box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(typ);
        out.extend_from_slice(payload);
        out
    }

    fn mp3_fixture_with_id3_xing() -> Vec<u8> {
        let header = [0xff, 0xfb, 0x90, 0x64];
        let frame_len = parse_mp3_frame_header(&header, 0).unwrap().frame_len;
        let mut out = b"ID3\x04\0\0".to_vec();
        out.extend_from_slice(&id3_synchsafe(16));
        out.extend_from_slice(&[0u8; 16]);
        for index in 0..4 {
            let mut frame = vec![0u8; frame_len];
            frame[0..4].copy_from_slice(&header);
            if index == 0 {
                let xing_offset = 36usize;
                frame[xing_offset..xing_offset + 4].copy_from_slice(b"Xing");
                frame[xing_offset + 4..xing_offset + 8].copy_from_slice(&0x3u32.to_be_bytes());
                frame[xing_offset + 8..xing_offset + 12].copy_from_slice(&4u32.to_be_bytes());
                frame[xing_offset + 12..xing_offset + 16]
                    .copy_from_slice(&((frame_len * 4) as u32).to_be_bytes());
            }
            out.extend_from_slice(&frame);
        }
        out
    }

    fn id3_synchsafe(value: u32) -> [u8; 4] {
        [
            ((value >> 21) & 0x7f) as u8,
            ((value >> 14) & 0x7f) as u8,
            ((value >> 7) & 0x7f) as u8,
            (value & 0x7f) as u8,
        ]
    }

    fn j2k_codestream_fixture() -> Vec<u8> {
        let mut out = vec![0xff, 0x4f, 0xff, 0x51];
        let mut siz = vec![0u8; 37];
        siz[2..6].copy_from_slice(&800u32.to_be_bytes());
        siz[6..10].copy_from_slice(&600u32.to_be_bytes());
        siz[34..36].copy_from_slice(&3u16.to_be_bytes());
        siz[36] = 7;
        out.extend_from_slice(&39u16.to_be_bytes());
        out.extend_from_slice(&siz);
        out.extend_from_slice(&[0xff, 0x52]);
        let mut cod = vec![0u8; 10];
        cod[1] = 0;
        cod[9] = 1;
        out.extend_from_slice(&12u16.to_be_bytes());
        out.extend_from_slice(&cod);
        out.extend_from_slice(&[0xff, 0x90]);
        out
    }

    fn wav_fixture() -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&44100u32.to_le_bytes());
        fmt.extend_from_slice(&176400u32.to_le_bytes());
        fmt.extend_from_slice(&4u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());
        let mut data = Vec::new();
        data.extend_from_slice(b"fmt ");
        data.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        data.extend_from_slice(&fmt);
        data.extend_from_slice(b"data");
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, 0]);
        let mut riff = Vec::new();
        riff.extend_from_slice(b"RIFF");
        riff.extend_from_slice(&((data.len() + 4) as u32).to_le_bytes());
        riff.extend_from_slice(b"WAVE");
        riff.extend_from_slice(&data);
        riff
    }

    fn synthetic_matroska() -> Vec<u8> {
        let ebml = ebml_master(0x1a45dfa3, &[ebml_string_elem(0x4282, "webm")].concat());
        let video = ebml_master(
            0xe0,
            &[ebml_uint_elem(0xb0, 640), ebml_uint_elem(0xba, 360)].concat(),
        );
        let track = ebml_master(
            0xae,
            &[
                ebml_uint_elem(0xd7, 1),
                ebml_uint_elem(0x83, 1),
                ebml_string_elem(0x86, "V_VP9"),
                video,
            ]
            .concat(),
        );
        let tracks = ebml_master(0x1654ae6b, &track);
        let segment = ebml_master(0x18538067, &tracks);
        [ebml, segment].concat()
    }

    fn ebml_master(id: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = ebml_id(id);
        out.extend_from_slice(&ebml_size(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn ebml_string_elem(id: u64, value: &str) -> Vec<u8> {
        let mut out = ebml_id(id);
        out.extend_from_slice(&ebml_size(value.len() as u64));
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn ebml_uint_elem(id: u64, value: u64) -> Vec<u8> {
        let bytes = if value <= 0xff {
            vec![value as u8]
        } else if value <= 0xffff {
            (value as u16).to_be_bytes().to_vec()
        } else {
            (value as u32).to_be_bytes().to_vec()
        };
        let mut out = ebml_id(id);
        out.extend_from_slice(&ebml_size(bytes.len() as u64));
        out.extend_from_slice(&bytes);
        out
    }

    fn ebml_id(id: u64) -> Vec<u8> {
        if id <= 0xff {
            vec![id as u8]
        } else if id <= 0xffff {
            (id as u16).to_be_bytes().to_vec()
        } else if id <= 0xff_ffff {
            vec![(id >> 16) as u8, (id >> 8) as u8, id as u8]
        } else {
            (id as u32).to_be_bytes().to_vec()
        }
    }

    fn ebml_size(size: u64) -> Vec<u8> {
        if size < 0x7f {
            vec![0x80 | size as u8]
        } else if size < 0x3fff {
            let value = 0x4000 | size as u16;
            value.to_be_bytes().to_vec()
        } else {
            let value = 0x0020_0000 | size as u32;
            vec![(value >> 16) as u8, (value >> 8) as u8, value as u8]
        }
    }
}

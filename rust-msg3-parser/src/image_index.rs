use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
#[cfg(windows)]
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, UNIX_EPOCH};

use base64::Engine;
use crossbeam_channel::{unbounded, Receiver, Sender};
#[cfg(windows)]
use image::RgbImage;
use image::{DynamicImage, GenericImageView, GrayImage, ImageDecoder, ImageReader};
use rusqlite::config::DbConfig;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::config;
use crate::msg3_log_service_asset_http::asset_href;

#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, GetFileInformationByHandleEx, FILE_ATTRIBUTE_DIRECTORY, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FIND_FIRST_EX_LARGE_FETCH, OPEN_EXISTING, WIN32_FIND_DATAW,
};

const PHASH_SIZE: u32 = 32;
const PHASH_SIZE_USIZE: usize = PHASH_SIZE as usize;
const PHASH_LOW: usize = 8;
const PHASH_ALGO_LEGACY: &str = "phash_v1_triangle32_legacy";
const PHASH_ALGO_V2: &str = "phash_v2_fullframe_triangle32_whitealpha_oriented";
const BLUR_ALGO_V2: &str = "blur_v2_luma256_gradient";
const FINGERPRINT_VERSION_V2: &str = "fingerprint_v2_fullframe_tile_blur256";
const TILE_ALGO_V1: &str = "tile_hash_v1_triangle32";
const BLUR_INPUT_MAX_SIDE: u32 = 256;
const TILE_STRICT_HAMMING_THRESHOLD: u32 = 6;
const TILE_DEFAULT_HAMMING_THRESHOLD: u32 = 8;
const VECTOR_SKETCH_BITS: usize = 64;
const VECTOR_BUCKET_BITS: u32 = 12;
#[cfg(test)]
const VECTOR_PROBE_MAX_HAMMING: u32 = 1;
const DEFAULT_MAX_FILES: usize = 200_000;
const DEFAULT_HAMMING_THRESHOLD: u32 = 10;
const DEFAULT_CPU_CLIP_BATCH_SIZE: usize = 8;
const DEFAULT_DIRECTML_CLIP_BATCH_SIZE: usize = 64;
const DEFAULT_CUDA_CLIP_BATCH_SIZE: usize = 128;
const DEFAULT_MANIFEST_BATCH_SIZE: usize = 1024;
const DEFAULT_MANIFEST_WORKERS_CAP: usize = 32;
const MAX_MANIFEST_WORKERS: usize = 64;
const POPULARITY_CACHE_SCHEMA_VERSION: i64 = 3;
#[cfg(feature = "image-index-sscd")]
const DEFAULT_SSCD_PREPROCESS_WORKERS_CAP: usize = 8;
#[cfg(feature = "image-index-sscd")]
const MAX_SSCD_PREPROCESS_WORKERS: usize = 32;
#[cfg(feature = "image-index-sscd")]
const SSCD_ORT_PROFILE_ENV: &str = "QQ_ANALYZER_SSCD_ORT_PROFILE";
#[cfg(feature = "image-index-sscd")]
const SSCD_RESIZE_BACKEND_ENV: &str = "QQ_ANALYZER_SSCD_RESIZE_BACKEND";
#[cfg(feature = "image-index-sscd")]
const SSCD_IO_BINDING_ENV: &str = "QQ_ANALYZER_SSCD_IO_BINDING";
#[cfg(feature = "image-index-clip")]
const CLIP_ORT_INTRA_THREADS_ENV: &str = "QQ_ANALYZER_CLIP_ORT_INTRA_THREADS";
#[cfg(feature = "image-index-sscd")]
const SSCD_ORT_INTRA_THREADS_ENV: &str = "QQ_ANALYZER_SSCD_ORT_INTRA_THREADS";
const PREFETCH_EMBED_INPUTS_ENV: &str = "QQ_ANALYZER_PREFETCH_EMBED_INPUTS";
const PREPARE_PREFETCH_DEPTH_ENV: &str = "QQ_ANALYZER_PREPARE_PREFETCH_DEPTH";
const EMBEDDING_KIND_ENV: &str = "QQ_ANALYZER_EMBEDDING_KIND";
const ASYNC_SQLITE_WRITER_ENV: &str = "QQ_ANALYZER_ASYNC_SQLITE_WRITER";
#[cfg(all(windows, feature = "image-index-clip"))]
const CLIP_SCALED_JPEG_DECODE_ENV: &str = "QQ_ANALYZER_CLIP_SCALED_JPEG_DECODE";
#[cfg(all(windows, feature = "image-index-clip"))]
const CLIP_SCALED_JPEG_MIN_EDGE_ENV: &str = "QQ_ANALYZER_CLIP_SCALED_JPEG_MIN_EDGE";
#[cfg(all(windows, feature = "image-index-sscd"))]
const SSCD_SCALED_JPEG_DECODE_ENV: &str = "QQ_ANALYZER_SSCD_SCALED_JPEG_DECODE";
#[cfg(all(windows, feature = "image-index-sscd"))]
const SSCD_SCALED_JPEG_MIN_EDGE_ENV: &str = "QQ_ANALYZER_SSCD_SCALED_JPEG_MIN_EDGE";
#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
const CUDA_CONV_ALGO_ENV: &str = "QQ_ANALYZER_CUDA_CONV_ALGO";
#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
const CUDA_MAX_WORKSPACE_ENV: &str = "QQ_ANALYZER_CUDA_MAX_WORKSPACE";
#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
const CUDA_TF32_ENV: &str = "QQ_ANALYZER_CUDA_TF32";
#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
const CUDA_PREFER_NHWC_ENV: &str = "QQ_ANALYZER_CUDA_PREFER_NHWC";
#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
const CUDA_FUSE_CONV_BIAS_ENV: &str = "QQ_ANALYZER_CUDA_FUSE_CONV_BIAS";
#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
const CUDA_GRAPH_ENV: &str = "QQ_ANALYZER_CUDA_GRAPH";
#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
const CUDA_GRAPH_LEGACY_ENV: &str = "QQ_ANALYZER_UNSAFE_CUDA_GRAPH";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_FP16_ENV: &str = "QQ_ANALYZER_TENSORRT_FP16";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_INT8_ENV: &str = "QQ_ANALYZER_TENSORRT_INT8";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_ENGINE_CACHE_ENV: &str = "QQ_ANALYZER_TENSORRT_ENGINE_CACHE";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_ENGINE_CACHE_PATH_ENV: &str = "QQ_ANALYZER_TENSORRT_ENGINE_CACHE_PATH";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_MAX_WORKSPACE_ENV: &str = "QQ_ANALYZER_TENSORRT_MAX_WORKSPACE";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_MIN_SUBGRAPH_ENV: &str = "QQ_ANALYZER_TENSORRT_MIN_SUBGRAPH";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_BUILDER_OPT_LEVEL_ENV: &str = "QQ_ANALYZER_TENSORRT_BUILDER_OPT_LEVEL";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_CUDA_GRAPH_ENV: &str = "QQ_ANALYZER_TENSORRT_CUDA_GRAPH";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_DUMP_SUBGRAPHS_ENV: &str = "QQ_ANALYZER_TENSORRT_DUMP_SUBGRAPHS";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_DETAILED_BUILD_LOG_ENV: &str = "QQ_ANALYZER_TENSORRT_DETAILED_BUILD_LOG";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_ONNX_MODEL_FOLDER_ENV: &str = "QQ_ANALYZER_TENSORRT_ONNX_MODEL_FOLDER";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_PROFILE_MIN_SHAPES_ENV: &str = "QQ_ANALYZER_TENSORRT_PROFILE_MIN_SHAPES";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_PROFILE_OPT_SHAPES_ENV: &str = "QQ_ANALYZER_TENSORRT_PROFILE_OPT_SHAPES";
#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
const TENSORRT_PROFILE_MAX_SHAPES_ENV: &str = "QQ_ANALYZER_TENSORRT_PROFILE_MAX_SHAPES";
const MAX_QUERY_RESULTS: usize = 1000;
const QUERY_CANDIDATE_MULTIPLIER: usize = 4;
const FAST_SEMANTIC_VECTOR_SCAN_CAP: usize = 8192;
const SSCD_SAME_IMAGE_THRESHOLD: f64 = 0.98;
const IMAGE_WALK_ENTRY_BUDGET: usize = 512;
const FAST_MANIFEST_PENDING_FLAG: &str = "manifest_pending";
const SMALL_IMAGE_EDGE: u32 = 128;
const TINY_IMAGE_EDGE: u32 = 64;

fn trace_timing_enabled() -> bool {
    env::var_os("QQ_ANALYZER_TRACE_TIMING").is_some()
}

fn trace_timing(started: Instant, label: &str) {
    if trace_timing_enabled() {
        eprintln!(
            "TIMING image_index elapsed_ms={} {label}",
            started.elapsed().as_millis()
        );
    }
}

#[cfg(feature = "image-index-sscd")]
const SSCD_INPUT_SIZE: u32 = 320;
#[cfg(windows)]
const TURBOJPEG_VERSION_NUMBER: c_int = 3_002_000;
#[cfg(windows)]
const TJINIT_DECOMPRESS: c_int = 1;
#[cfg(windows)]
const TJPF_RGB: c_int = 0;
#[cfg(windows)]
const TJPF_GRAY: c_int = 6;
#[cfg(windows)]
const TJPARAM_JPEGWIDTH: c_int = 5;
#[cfg(windows)]
const TJPARAM_JPEGHEIGHT: c_int = 6;
#[cfg(windows)]
const TJPARAM_LOSSLESS: c_int = 15;

#[cfg(any(windows, test))]
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JpegScalingFactor {
    num: i32,
    denom: i32,
}

#[cfg(windows)]
type TjHandle = *mut c_void;

#[cfg(windows)]
type Tj3InitVersion = unsafe extern "C" fn(c_int, c_int) -> TjHandle;
#[cfg(windows)]
type Tj3Destroy = unsafe extern "C" fn(TjHandle);
#[cfg(windows)]
type Tj3GetErrorStr = unsafe extern "C" fn(TjHandle) -> *mut c_char;
#[cfg(windows)]
type Tj3Get = unsafe extern "C" fn(TjHandle, c_int) -> c_int;
#[cfg(windows)]
type Tj3GetScalingFactors = unsafe extern "C" fn(*mut c_int) -> *mut JpegScalingFactor;
#[cfg(windows)]
type Tj3SetScalingFactor = unsafe extern "C" fn(TjHandle, JpegScalingFactor) -> c_int;
#[cfg(windows)]
type Tj3DecompressHeader = unsafe extern "C" fn(TjHandle, *const u8, usize) -> c_int;
#[cfg(windows)]
type Tj3Decompress8 =
    unsafe extern "C" fn(TjHandle, *const u8, usize, *mut u8, c_int, c_int) -> c_int;

#[cfg(windows)]
struct TurboJpegApi {
    _module: usize,
    tj3_init_version: Tj3InitVersion,
    tj3_destroy: Tj3Destroy,
    tj3_get_error_str: Tj3GetErrorStr,
    tj3_get: Tj3Get,
    tj3_get_scaling_factors: Tj3GetScalingFactors,
    tj3_set_scaling_factor: Tj3SetScalingFactor,
    tj3_decompress_header: Tj3DecompressHeader,
    tj3_decompress8: Tj3Decompress8,
}

#[cfg(windows)]
struct TurboJpegHandle<'a> {
    api: &'a TurboJpegApi,
    handle: TjHandle,
}

#[cfg(windows)]
impl Drop for TurboJpegHandle<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.tj3_destroy)(self.handle);
        }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
    fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
}

fn default_clip_batch_size(execution_provider: &str) -> usize {
    match execution_provider.to_ascii_lowercase().as_str() {
        "cuda" => DEFAULT_CUDA_CLIP_BATCH_SIZE,
        "tensorrt" | "trt" => DEFAULT_CUDA_CLIP_BATCH_SIZE,
        "directml" => DEFAULT_DIRECTML_CLIP_BATCH_SIZE,
        "auto" if cfg!(feature = "image-index-clip-directml") => DEFAULT_DIRECTML_CLIP_BATCH_SIZE,
        "auto" if cfg!(feature = "image-index-clip-cuda") => DEFAULT_CUDA_CLIP_BATCH_SIZE,
        _ => DEFAULT_CPU_CLIP_BATCH_SIZE,
    }
}

#[cfg(windows)]
fn turbojpeg_api() -> Option<&'static TurboJpegApi> {
    static API: OnceLock<Option<TurboJpegApi>> = OnceLock::new();
    API.get_or_init(load_turbojpeg_api).as_ref()
}

#[cfg(windows)]
fn load_turbojpeg_api() -> Option<TurboJpegApi> {
    let dll = env::var_os("QQ_ANALYZER_TURBOJPEG_DLL")?;
    let wide = OsString::from(dll)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        let module = LoadLibraryW(wide.as_ptr());
        if module.is_null() {
            return None;
        }
        Some(TurboJpegApi {
            _module: module as usize,
            tj3_init_version: load_turbojpeg_proc(module, b"tj3InitVersion\0")?,
            tj3_destroy: load_turbojpeg_proc(module, b"tj3Destroy\0")?,
            tj3_get_error_str: load_turbojpeg_proc(module, b"tj3GetErrorStr\0")?,
            tj3_get: load_turbojpeg_proc(module, b"tj3Get\0")?,
            tj3_get_scaling_factors: load_turbojpeg_proc(module, b"tj3GetScalingFactors\0")?,
            tj3_set_scaling_factor: load_turbojpeg_proc(module, b"tj3SetScalingFactor\0")?,
            tj3_decompress_header: load_turbojpeg_proc(module, b"tj3DecompressHeader\0")?,
            tj3_decompress8: load_turbojpeg_proc(module, b"tj3Decompress8\0")?,
        })
    }
}

#[cfg(windows)]
unsafe fn load_turbojpeg_proc<T: Copy>(module: *mut c_void, name: &[u8]) -> Option<T> {
    let ptr = GetProcAddress(module, name.as_ptr());
    if ptr.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&ptr))
    }
}

#[cfg(windows)]
fn turbojpeg_error(api: &TurboJpegApi, handle: TjHandle) -> String {
    unsafe {
        let ptr = (api.tj3_get_error_str)(handle);
        if ptr.is_null() {
            "TurboJPEG error".to_string()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

#[cfg(windows)]
fn ensure_turbojpeg_benchmark_backend() -> anyhow::Result<()> {
    if turbojpeg_api().is_some() {
        Ok(())
    } else {
        anyhow::bail!(
            "TurboJPEG benchmark mode requires QQ_ANALYZER_TURBOJPEG_DLL to point to a loadable turbojpeg.dll"
        )
    }
}

#[cfg(not(windows))]
fn ensure_turbojpeg_benchmark_backend() -> anyhow::Result<()> {
    anyhow::bail!("TurboJPEG benchmark modes require a Windows build with turbojpeg.dll")
}

fn normalize_query_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_QUERY_RESULTS)
}

fn expanded_query_limit(limit: usize) -> usize {
    normalize_query_limit(limit).saturating_mul(QUERY_CANDIDATE_MULTIPLIER)
}

fn default_manifest_workers() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, DEFAULT_MANIFEST_WORKERS_CAP)
}

fn fs_path_for_host(path: &Path) -> Cow<'_, Path> {
    #[cfg(not(windows))]
    {
        if let Some(converted) = wsl_mount_path_from_windows_path(path) {
            return Cow::Owned(converted);
        }
    }
    Cow::Borrowed(path)
}

/// Converts a manifest path into the filesystem spelling used by the current
/// host. Manifests built on Windows commonly retain extended `\\?\D:\...`
/// paths, which must be mapped to `/mnt/d/...` when the service runs in WSL.
pub fn manifest_path_for_host(path: &Path) -> PathBuf {
    fs_path_for_host(path).into_owned()
}

#[cfg(not(windows))]
fn wsl_mount_path_from_windows_path(path: &Path) -> Option<PathBuf> {
    let text = path.to_string_lossy();
    let mut value = text.as_ref();
    if let Some(rest) = value.strip_prefix("\\\\?\\") {
        value = rest;
    } else if let Some(rest) = value.strip_prefix("\\??\\") {
        value = rest;
    }

    let bytes = value.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return None;
    }

    let drive = char::from(bytes[0]).to_ascii_lowercase();
    let mut converted = PathBuf::from(format!("/mnt/{drive}"));
    for part in value[3..].split(['\\', '/']) {
        if !part.is_empty() {
            converted.push(part);
        }
    }
    Some(converted)
}

fn fs_metadata_for_host(path: &Path) -> io::Result<fs::Metadata> {
    fs::metadata(fs_path_for_host(path).as_ref())
}

fn fs_read_for_host(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(fs_path_for_host(path).as_ref())
}

fn fs_file_open_for_host(path: &Path) -> io::Result<fs::File> {
    fs::File::open(fs_path_for_host(path).as_ref())
}

fn manifest_write_batch_size(options: &BuildOptions) -> usize {
    options.clip_batch_size.max(DEFAULT_MANIFEST_BATCH_SIZE)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManifestMode {
    #[default]
    Full,
    Fast,
}

impl ManifestMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "full" | "decode" | "quality" => Ok(Self::Full),
            "fast" | "metadata" | "metadata-only" | "deferred" | "defer-quality" => Ok(Self::Fast),
            _ => anyhow::bail!("unknown manifest mode: {value}; expected full or fast"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Fast => "fast",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub root: PathBuf,
    pub account: String,
    pub asset_roots: Vec<PathBuf>,
    pub manifest_path: Option<PathBuf>,
    pub max_files: usize,
    pub force: bool,
    pub pipeline: String,
    pub stage: BuildStage,
    pub model_dir: Option<PathBuf>,
    pub sscd_model_dir: Option<PathBuf>,
    pub clip_model: String,
    pub sscd_model: String,
    pub execution_provider: String,
    pub backend: String,
    pub clip_batch_size: usize,
    pub manifest_workers: usize,
    pub manifest_mode: ManifestMode,
}

impl BuildOptions {
    pub fn normalized(mut self) -> Self {
        if self.max_files == 0 {
            self.max_files = DEFAULT_MAX_FILES;
        }
        if self.pipeline.is_empty() {
            self.pipeline = "full".to_string();
        }
        if self.clip_model.is_empty() {
            self.clip_model = "mobileclip2-s2".to_string();
        }
        if self.sscd_model.is_empty() {
            self.sscd_model = "sscd_disc_mixup".to_string();
        }
        if self.execution_provider.is_empty() {
            self.execution_provider = "auto".to_string();
        }
        if self.backend.is_empty() {
            self.backend = "manifest-sqlite".to_string();
        }
        if self.clip_batch_size == 0 {
            self.clip_batch_size = default_clip_batch_size(&self.execution_provider);
        }
        if self.manifest_workers == 0 {
            self.manifest_workers = default_manifest_workers();
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BuildStage {
    Manifest,
    Embeddings,
    #[default]
    All,
}

impl BuildStage {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "all" => Ok(Self::All),
            "manifest" | "scan" | "hash" | "hashes" => Ok(Self::Manifest),
            "embeddings" | "embedding" | "vectors" | "vector" => Ok(Self::Embeddings),
            _ => anyhow::bail!(
                "unknown image-index build stage: {value}; expected manifest, embeddings, or all"
            ),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Embeddings => "embeddings",
            Self::All => "all",
        }
    }

    fn wants_manifest(&self) -> bool {
        matches!(self, Self::Manifest | Self::All)
    }

    fn wants_embeddings(&self) -> bool {
        matches!(self, Self::Embeddings | Self::All)
    }
}

#[derive(Clone, Debug)]
pub struct QueryImageOptions {
    pub root: PathBuf,
    pub account: String,
    pub manifest_path: Option<PathBuf>,
    pub image: PathBuf,
    pub mode: QueryMode,
    pub query_strategy: QueryStrategy,
    pub limit: usize,
    pub hamming_threshold: u32,
    pub model_dir: Option<PathBuf>,
    pub sscd_model_dir: Option<PathBuf>,
    pub clip_model: String,
    pub sscd_model: String,
    pub execution_provider: String,
}

#[derive(Clone, Debug)]
pub struct QueryTextOptions {
    pub root: PathBuf,
    pub account: String,
    pub manifest_path: Option<PathBuf>,
    pub text: String,
    pub query_strategy: QueryStrategy,
    pub limit: usize,
    pub model_dir: Option<PathBuf>,
    pub clip_model: String,
    pub execution_provider: String,
}

#[derive(Clone, Debug)]
pub struct ManifestBenchmarkOptions {
    pub root: PathBuf,
    pub account: String,
    pub asset_roots: Vec<PathBuf>,
    pub max_files: usize,
    pub mode: ManifestBenchmarkMode,
    pub header_bytes: usize,
    pub sqlite_path: Option<PathBuf>,
    pub batch_size: usize,
}

impl ManifestBenchmarkOptions {
    pub fn normalized(mut self) -> Self {
        if self.max_files == 0 {
            self.max_files = DEFAULT_MAX_FILES;
        }
        if self.header_bytes == 0 {
            self.header_bytes = 4096;
        }
        if self.batch_size == 0 {
            self.batch_size = 1000;
        }
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestBenchmarkMode {
    EnumOnly,
    EnumPlusOpen0,
    EnumPlusFileInfo,
    EnumPlusHeader,
    EnumPlusDecode,
    FullManifestNoSqlite,
    FingerprintNoSqlite,
    TurboJpegFingerprintNoSqlite,
    SqliteBatch,
    DecodeProfile,
    JpegLumaDecodeProfile,
    TurboJpegDecodeProfile,
}

impl ManifestBenchmarkMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "enum_only" | "enum-only" | "enum" => Ok(Self::EnumOnly),
            "enum_plus_open0" | "enum-plus-open0" | "open0" => Ok(Self::EnumPlusOpen0),
            "enum_plus_fileinfo" | "enum-plus-fileinfo" | "fileinfo" => {
                Ok(Self::EnumPlusFileInfo)
            }
            "enum_plus_header" | "enum-plus-header" | "header" => Ok(Self::EnumPlusHeader),
            "enum_plus_decode" | "enum-plus-decode" | "decode" => Ok(Self::EnumPlusDecode),
            "full_manifest_no_sqlite" | "full-manifest-no-sqlite" | "no_sqlite"
            | "no-sqlite" => Ok(Self::FullManifestNoSqlite),
            "fingerprint_no_sqlite" | "fingerprint-no-sqlite" | "fast_fingerprint_no_sqlite"
            | "fast-fingerprint-no-sqlite" | "current_manifest_no_sqlite"
            | "current-manifest-no-sqlite" => Ok(Self::FingerprintNoSqlite),
            "turbojpeg_fingerprint_no_sqlite"
            | "turbojpeg-fingerprint-no-sqlite"
            | "libjpeg_turbo_fingerprint_no_sqlite"
            | "libjpeg-turbo-fingerprint-no-sqlite"
            | "tj_fingerprint_no_sqlite"
            | "tj-fingerprint-no-sqlite" => Ok(Self::TurboJpegFingerprintNoSqlite),
            "sqlite_batch" | "sqlite-batch" | "sqlite" => Ok(Self::SqliteBatch),
            "decode_profile" | "decode-profile" | "profile" | "format-profile" => {
                Ok(Self::DecodeProfile)
            }
            "jpeg_luma_decode_profile" | "jpeg-luma-decode-profile"
            | "zune_jpeg_luma_decode_profile" | "zune-jpeg-luma-decode-profile" => {
                Ok(Self::JpegLumaDecodeProfile)
            }
            "turbojpeg_decode_profile" | "turbojpeg-decode-profile"
            | "libjpeg_turbo_decode_profile" | "libjpeg-turbo-decode-profile"
            | "tj_decode_profile" | "tj-decode-profile" => Ok(Self::TurboJpegDecodeProfile),
            _ => anyhow::bail!(
                "unknown manifest benchmark mode: {value}; expected enum_only, enum_plus_open0, enum_plus_fileinfo, enum_plus_header, enum_plus_decode, full_manifest_no_sqlite, fingerprint_no_sqlite, turbojpeg_fingerprint_no_sqlite, sqlite_batch, decode_profile, jpeg_luma_decode_profile, or turbojpeg_decode_profile"
            ),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EnumOnly => "enum_only",
            Self::EnumPlusOpen0 => "enum_plus_open0",
            Self::EnumPlusFileInfo => "enum_plus_fileinfo",
            Self::EnumPlusHeader => "enum_plus_header",
            Self::EnumPlusDecode => "enum_plus_decode",
            Self::FullManifestNoSqlite => "full_manifest_no_sqlite",
            Self::FingerprintNoSqlite => "fingerprint_no_sqlite",
            Self::TurboJpegFingerprintNoSqlite => "turbojpeg_fingerprint_no_sqlite",
            Self::SqliteBatch => "sqlite_batch",
            Self::DecodeProfile => "decode_profile",
            Self::JpegLumaDecodeProfile => "jpeg_luma_decode_profile",
            Self::TurboJpegDecodeProfile => "turbojpeg_decode_profile",
        }
    }

    fn requires_turbojpeg_backend(&self) -> bool {
        matches!(
            self,
            Self::TurboJpegFingerprintNoSqlite | Self::TurboJpegDecodeProfile
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryMode {
    Exact,
    Near,
    Patch,
    Screenshot,
    Strict,
    Recall,
    Copy,
    Semantic,
    All,
}

impl QueryMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "all" => Ok(Self::All),
            "exact" => Ok(Self::Exact),
            "near" | "near-hash" | "phash" | "pdq" => Ok(Self::Near),
            "patch" | "crop" | "local" => Ok(Self::Patch),
            "screenshot" | "screen" => Ok(Self::Screenshot),
            "strict" => Ok(Self::Strict),
            "recall" => Ok(Self::Recall),
            "copy" | "sscd" => Ok(Self::Copy),
            "semantic" | "clip" => Ok(Self::Semantic),
            _ => anyhow::bail!(
                "unknown image query mode: {value}; expected exact, near, patch, screenshot, strict, recall, copy, semantic, or all"
            ),
        }
    }

    fn wants_exact(&self) -> bool {
        matches!(self, Self::Exact | Self::Strict | Self::Recall | Self::All)
    }

    fn wants_near(&self) -> bool {
        matches!(self, Self::Near | Self::Strict | Self::Recall | Self::All)
    }

    fn wants_tile(&self) -> bool {
        matches!(
            self,
            Self::Near | Self::Patch | Self::Screenshot | Self::Recall | Self::All
        )
    }

    fn wants_copy(&self) -> bool {
        matches!(
            self,
            Self::Copy | Self::Screenshot | Self::Recall | Self::All
        )
    }

    fn wants_semantic(&self) -> bool {
        matches!(
            self,
            Self::Semantic | Self::Screenshot | Self::Recall | Self::All
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueryStrategy {
    #[default]
    Exact,
    Fast,
}

impl QueryStrategy {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "exact" | "full" | "full-scan" => Ok(Self::Exact),
            "fast" | "bucket" | "shortlist" | "approx" | "approximate" => Ok(Self::Fast),
            _ => anyhow::bail!("unknown image query strategy: {value}; expected exact or fast"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Fast => "fast",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BuildReport {
    pub account: String,
    pub pipeline: String,
    pub stage: String,
    pub index_root: String,
    pub manifest: String,
    pub roots: Vec<String>,
    pub root_stats: Vec<RootScanReport>,
    pub scanned_files: usize,
    pub indexed_files: usize,
    pub embedded_files: usize,
    pub reused_embedding_files: usize,
    pub unchanged_files: usize,
    pub skipped_files: usize,
    pub error_files: usize,
    pub stale_files: usize,
    pub max_files: usize,
    pub clip_batch_size: usize,
    pub manifest_workers: usize,
    pub manifest_mode: String,
    pub backfilled_embeddings: usize,
    pub canonicalized_embeddings: usize,
    pub elapsed_ms: u64,
    pub quality: AssetQualityStats,
    pub model_status: ModelStatus,
    pub embedding_profile: Option<EmbeddingProfileReport>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EmbeddingProfileReport {
    pub iterations: usize,
    pub batches: usize,
    pub images: usize,
    pub reuse_ms: f64,
    pub pending_query_ms: f64,
    pub prepare_ms: f64,
    pub prepare_workers: usize,
    pub prepare_pool_threads_started: usize,
    pub prepare_pool_persistent: bool,
    pub prepare_dynamic_scheduling: bool,
    pub prepare_prefetch_depth: usize,
    pub prepare_jobs: usize,
    pub prepare_metadata_ms: f64,
    pub prepare_existing_ms: f64,
    pub prepare_read_ms: f64,
    pub prepare_sha256_ms: f64,
    pub prepare_jpeg_orientation_ms: f64,
    pub prepare_jpeg_luma_ms: f64,
    pub prepare_dynamic_decode_ms: f64,
    pub prepare_keep_image_decode_ms: f64,
    pub prepare_scaled_jpeg_decode_ms: f64,
    pub prepare_scaled_jpeg_decode_files: usize,
    pub prepare_scaled_jpeg_decode_fallbacks: usize,
    pub prepare_scaled_jpeg_target_edge: u32,
    pub prepare_scaled_jpeg_source_pixels: u64,
    pub prepare_scaled_jpeg_output_pixels: u64,
    pub prepare_luma_ms: f64,
    pub prepare_fingerprint_core_ms: f64,
    pub prepare_tile_hash_ms: f64,
    pub prepare_reused_fingerprints: usize,
    pub prepare_errors: usize,
    pub flush_ms: f64,
    pub load_clip_ms: f64,
    pub load_sscd_ms: f64,
    pub clip_batches: usize,
    pub clip_images: usize,
    pub clip_ms: f64,
    pub clip_preprocess_ms: f64,
    pub clip_preprocess_resize_cpu_ms: f64,
    pub clip_preprocess_normalize_cpu_ms: f64,
    pub clip_tensor_ms: f64,
    pub clip_run_ms: f64,
    pub clip_extract_ms: f64,
    pub clip_sscd_serial_batches: usize,
    pub clip_sscd_serial_ms: f64,
    pub sscd_preprocess_wait_after_clip_ms: f64,
    pub concurrent_gpu_batches: usize,
    pub concurrent_gpu_ms: f64,
    pub prefetch_embed_input_batches: usize,
    pub prefetch_embed_input_ms: f64,
    pub prefetch_embed_input_wait_ms: f64,
    pub clip_ort_profile_path: Option<String>,
    pub clip_ort_profile_output_path: Option<String>,
    pub clip_ort_profile_error: Option<String>,
    pub sscd_batches: usize,
    pub sscd_images: usize,
    pub sscd_preprocess_workers: usize,
    pub sscd_preprocess_ms: f64,
    pub sscd_input_resize_cpu_ms: f64,
    pub sscd_input_normalize_cpu_ms: f64,
    pub sscd_tensor_ms: f64,
    pub sscd_run_ms: f64,
    pub sscd_extract_ms: f64,
    pub sscd_normalize_ms: f64,
    pub sscd_total_ms: f64,
    pub sscd_ort_profile_path: Option<String>,
    pub sscd_ort_profile_output_path: Option<String>,
    pub sscd_ort_profile_error: Option<String>,
    pub sqlite_ms: f64,
    pub fallback_batches: usize,
    pub fallback_images: usize,
    pub fallback_ms: f64,
    pub record_error_ms: f64,
}

#[derive(Default)]
struct EmbeddingProfiler {
    report: EmbeddingProfileReport,
}

impl EmbeddingProfiler {
    fn add_ms(slot: &mut f64, started: Instant) {
        *slot += elapsed_ms_f64(started);
    }

    fn merge_prepare(&mut self, other: EmbeddingProfileReport) {
        merge_prepare_profile(&mut self.report, other);
    }

    fn finish(self) -> EmbeddingProfileReport {
        self.report
    }
}

fn merge_prepare_profile(target: &mut EmbeddingProfileReport, other: EmbeddingProfileReport) {
    target.prepare_jobs += other.prepare_jobs;
    target.prepare_metadata_ms += other.prepare_metadata_ms;
    target.prepare_existing_ms += other.prepare_existing_ms;
    target.prepare_read_ms += other.prepare_read_ms;
    target.prepare_sha256_ms += other.prepare_sha256_ms;
    target.prepare_jpeg_orientation_ms += other.prepare_jpeg_orientation_ms;
    target.prepare_jpeg_luma_ms += other.prepare_jpeg_luma_ms;
    target.prepare_dynamic_decode_ms += other.prepare_dynamic_decode_ms;
    target.prepare_keep_image_decode_ms += other.prepare_keep_image_decode_ms;
    target.prepare_scaled_jpeg_decode_ms += other.prepare_scaled_jpeg_decode_ms;
    target.prepare_scaled_jpeg_decode_files += other.prepare_scaled_jpeg_decode_files;
    target.prepare_scaled_jpeg_decode_fallbacks += other.prepare_scaled_jpeg_decode_fallbacks;
    target.prepare_scaled_jpeg_target_edge = target
        .prepare_scaled_jpeg_target_edge
        .max(other.prepare_scaled_jpeg_target_edge);
    target.prepare_scaled_jpeg_source_pixels += other.prepare_scaled_jpeg_source_pixels;
    target.prepare_scaled_jpeg_output_pixels += other.prepare_scaled_jpeg_output_pixels;
    target.prepare_luma_ms += other.prepare_luma_ms;
    target.prepare_fingerprint_core_ms += other.prepare_fingerprint_core_ms;
    target.prepare_tile_hash_ms += other.prepare_tile_hash_ms;
    target.prepare_reused_fingerprints += other.prepare_reused_fingerprints;
    target.prepare_errors += other.prepare_errors;
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RootScanReport {
    pub root: String,
    pub directories_seen: usize,
    pub files_seen: usize,
    pub scanned_files: usize,
    pub indexed_files: usize,
    pub unchanged_files: usize,
    pub skipped_files: usize,
    pub error_files: usize,
    pub walk_errors: usize,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ManifestBenchmarkTiming {
    pub elapsed_find_ms: f64,
    pub elapsed_canonicalize_ms: f64,
    pub elapsed_metadata_ms: f64,
    pub elapsed_createfile_ms: f64,
    pub elapsed_fileinfo_ms: f64,
    pub elapsed_read_ms: f64,
    pub elapsed_decode_ms: f64,
    pub elapsed_hash_ms: f64,
    pub elapsed_phash_quality_ms: f64,
    pub elapsed_sqlite_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ManifestBenchmarkRootReport {
    pub root: String,
    pub directories_seen: usize,
    pub files_seen: usize,
    pub supported_files: usize,
    pub processed_files: usize,
    pub skipped_files: usize,
    pub error_files: usize,
    pub decode_errors: usize,
    pub open_errors: usize,
    pub fileinfo_errors: usize,
    pub read_errors: usize,
    pub walk_errors: usize,
    pub find_first_count: usize,
    pub find_next_count: usize,
    pub create_file_count: usize,
    pub fileinfo_count: usize,
    pub read_count: usize,
    pub decode_count: usize,
    pub sqlite_insert_count: usize,
    pub bytes_read: u64,
    pub elapsed_ms: u64,
    pub timing: ManifestBenchmarkTiming,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManifestBenchmarkReport {
    pub account: String,
    pub mode: String,
    pub enum_backend: String,
    pub roots: Vec<String>,
    pub max_files: usize,
    pub header_bytes: usize,
    pub sqlite_batch_size: usize,
    pub sqlite_path: Option<String>,
    pub root_stats: Vec<ManifestBenchmarkRootReport>,
    pub directories_seen: usize,
    pub files_seen: usize,
    pub supported_files: usize,
    pub processed_files: usize,
    pub skipped_files: usize,
    pub error_files: usize,
    pub decode_errors: usize,
    pub open_errors: usize,
    pub fileinfo_errors: usize,
    pub read_errors: usize,
    pub walk_errors: usize,
    pub find_first_count: usize,
    pub find_next_count: usize,
    pub create_file_count: usize,
    pub fileinfo_count: usize,
    pub read_count: usize,
    pub decode_count: usize,
    pub sqlite_insert_count: usize,
    pub bytes_read: u64,
    pub elapsed_ms: u64,
    pub timing: ManifestBenchmarkTiming,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_profile: Option<DecodeProfileReport>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DecodeProfileReport {
    pub by_format: Vec<DecodeProfileBucketReport>,
    pub by_format_megapixels: Vec<DecodeProfileBucketReport>,
    pub by_format_file_size: Vec<DecodeProfileBucketReport>,
    pub by_format_animation: Vec<DecodeProfileBucketReport>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DecodeProfileBucketReport {
    pub group: String,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub megapixels_bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub animation: Option<String>,
    pub count: usize,
    pub total_bytes: u64,
    pub total_pixels: u64,
    pub decode_ms_total: f64,
    pub decode_ms_avg: f64,
    pub decode_ms_p50: f64,
    pub decode_ms_p95: f64,
    pub decode_ms_p99: f64,
    pub bytes_p95: u64,
    pub pixels_p95: u64,
    pub animated_count: usize,
    pub error_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusReport {
    pub account: String,
    pub index_root: String,
    pub manifest: String,
    pub manifest_exists: bool,
    pub images: i64,
    pub stale_images: i64,
    pub errors: i64,
    pub exact_hashes: i64,
    pub perceptual_hashes: i64,
    pub embeddings: EmbeddingStats,
    pub quality: AssetQualityStats,
    pub model_status: ModelStatus,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EmbeddingStats {
    pub rows: i64,
    pub active_rows: i64,
    pub active_clip_rows: i64,
    pub active_sscd_rows: i64,
    pub active_rows_with_sketch: i64,
    pub active_rows_missing_sketch: i64,
    pub active_buckets: i64,
    pub active_images_missing_clip: i64,
    pub active_images_missing_sscd: i64,
}

#[derive(Clone, Debug)]
pub struct ImportEmbeddingsOptions {
    pub root: PathBuf,
    pub account: String,
    pub source_manifest: PathBuf,
    pub target_manifest: Option<PathBuf>,
    pub force: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportEmbeddingsReport {
    pub account: String,
    pub source_manifest: String,
    pub target_manifest: String,
    pub force: bool,
    pub source_rows: i64,
    pub target_active_images: i64,
    pub matching_rows: i64,
    pub imported_rows: usize,
    pub skipped_existing_rows: i64,
    pub backfilled_embeddings: usize,
    pub embeddings: EmbeddingStats,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AssetQualityStats {
    pub active_images: i64,
    pub duplicate_sha_groups: i64,
    pub duplicate_sha_files: i64,
    pub exact_phash_groups: i64,
    pub exact_phash_files: i64,
    pub blurry_files: i64,
    pub small_files: i64,
    pub tiny_files: i64,
    pub thumbnail_like_files: i64,
    pub manifest_pending_files: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueryReport {
    pub account: String,
    pub query: QueryDescriptor,
    pub mode: String,
    pub query_strategy: String,
    pub results: Vec<QueryResult>,
    pub unavailable: Vec<UnavailableSignal>,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueryDescriptor {
    pub path: String,
    pub sha256: String,
    pub phash: String,
    pub phash_algo: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueryResult {
    pub match_kind: String,
    pub score: f64,
    pub distance: Option<u32>,
    pub path: String,
    pub href: String,
    pub name: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sha256: String,
    pub phash: Option<String>,
    pub phash_algo: Option<String>,
    pub blur_score: Option<f64>,
    pub blur_algo: Option<String>,
    pub quality_flags: String,
    pub source_class: String,
    pub match_source: String,
    pub tile_algo: Option<String>,
    pub embedding_model: Option<String>,
    pub local_algo: Option<String>,
    pub matched_tile_count: Option<usize>,
    pub local_inliers: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TextQueryReport {
    pub account: String,
    pub text: String,
    pub query_strategy: String,
    pub limit: usize,
    pub results: Vec<QueryResult>,
    pub unavailable: Vec<UnavailableSignal>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnavailableSignal {
    pub signal: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelStatus {
    pub backend: String,
    pub clip_model: String,
    pub sscd_model: String,
    pub execution_provider: String,
    pub execution_provider_status: String,
    pub model_dir: Option<String>,
    pub sscd_model_path: Option<String>,
    pub exact_hash: String,
    pub near_hash: String,
    pub copy_descriptor: String,
    pub semantic_descriptor: String,
}

#[derive(Clone, Debug)]
struct ImageFingerprint {
    sha256: String,
    phash: String,
    phash_algo: String,
    width: u32,
    height: u32,
    blur_score: f64,
    blur_algo: String,
    quality_flags: String,
    source_class: String,
    detected_format: String,
    has_alpha: bool,
    orientation_applied: bool,
    fingerprint_version: String,
    tile_hashes: Vec<TileHash>,
}

#[derive(Clone, Debug)]
struct TileHash {
    tile_algo: String,
    tile_set: String,
    tile_index: i64,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    hash_hex: String,
}

#[derive(Clone, Debug)]
struct NormalizedLumaImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    detected_format: String,
    has_alpha: bool,
    orientation_applied: bool,
}

#[derive(Debug)]
struct PendingIndexImage {
    source_root: PathBuf,
    path: PathBuf,
    path_text: String,
    file_size: i64,
    mtime_unix: i64,
    fingerprint: ImageFingerprint,
    image: Option<DynamicImage>,
    reuse_existing_asset: bool,
}

#[derive(Clone, Debug)]
struct PendingEmbeddingAsset {
    path: PathBuf,
    source_root: PathBuf,
    file_size: i64,
    mtime_unix: i64,
    fingerprint: Option<ImageFingerprint>,
}

#[derive(Clone, Debug)]
struct IndexedImage {
    path: PathBuf,
    sha256: String,
    phash: Option<String>,
    phash_algo: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    blur_score: Option<f64>,
    blur_algo: Option<String>,
    quality_flags: String,
    source_class: String,
    tile_algo: Option<String>,
    matched_tile_count: Option<usize>,
}

#[derive(Default)]
pub struct QueryRuntimeCache {
    clip_vision: Option<CachedClipVisionRuntime>,
    clip_text: Option<CachedClipTextRuntime>,
    sscd: Option<CachedSscdRuntime>,
}

struct CachedClipVisionRuntime {
    key: RuntimeCacheKey,
    runtime: ClipVisionRuntime,
}

struct CachedClipTextRuntime {
    key: RuntimeCacheKey,
    runtime: ClipTextRuntime,
}

struct CachedSscdRuntime {
    key: RuntimeCacheKey,
    runtime: SscdRuntime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeCacheKey {
    model: String,
    path: PathBuf,
    execution_provider: String,
}

pub fn default_index_root(root: &Path, account: &str) -> PathBuf {
    config::account_output_root(root, account).join("image-index")
}

pub fn default_manifest_path(root: &Path, account: &str) -> PathBuf {
    default_index_root(root, account).join("manifest.sqlite")
}

pub fn build_index(options: BuildOptions) -> anyhow::Result<BuildReport> {
    let started = Instant::now();
    let options = options.normalized();
    trace_timing(started, "build_index:start");
    if !options.pipeline.trim().eq_ignore_ascii_case("full") {
        anyhow::bail!(
            "unknown image-index build pipeline: {}; expected full",
            options.pipeline
        );
    }
    let manifest = options
        .manifest_path
        .clone()
        .unwrap_or_else(|| default_manifest_path(&options.root, &options.account));
    let index_root = manifest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_index_root(&options.root, &options.account));
    fs::create_dir_all(&index_root)?;
    let mut con = Connection::open(&manifest)?;
    init_schema(&con)?;
    trace_timing(started, "build_index:init_schema_done");
    let roots = scan_roots(&options);
    let mut report = BuildReport {
        account: options.account.clone(),
        pipeline: options.pipeline.clone(),
        stage: options.stage.as_str().to_string(),
        index_root: index_root.display().to_string(),
        manifest: manifest.display().to_string(),
        roots: roots.iter().map(|p| p.display().to_string()).collect(),
        root_stats: Vec::new(),
        scanned_files: 0,
        indexed_files: 0,
        embedded_files: 0,
        reused_embedding_files: 0,
        unchanged_files: 0,
        skipped_files: 0,
        error_files: 0,
        stale_files: 0,
        max_files: options.max_files,
        clip_batch_size: options.clip_batch_size,
        manifest_workers: if options.manifest_mode == ManifestMode::Fast {
            1
        } else {
            options.manifest_workers
        },
        manifest_mode: options.manifest_mode.as_str().to_string(),
        backfilled_embeddings: 0,
        canonicalized_embeddings: 0,
        elapsed_ms: 0,
        quality: AssetQualityStats::default(),
        model_status: model_status(&options),
        embedding_profile: None,
    };

    let mut required_embeddings = Vec::new();
    if options.stage.wants_embeddings() {
        required_embeddings = required_embedding_keys(&options)?;
        trace_timing(started, "build_index:required_embeddings_done");
        report.backfilled_embeddings = backfill_embedding_metadata(&mut con)?;
        trace_timing(started, "build_index:backfill_embedding_metadata_done");
        report.canonicalized_embeddings =
            canonicalize_legacy_model_keys(&mut con, &required_embeddings)?;
        trace_timing(started, "build_index:canonicalize_legacy_model_keys_done");
    }

    if options.stage.wants_manifest() {
        trace_timing(started, "build_index:manifest_stage_start");
        build_manifest_stage(&mut con, &options, &roots, &mut report)?;
        let missing_exact_groups: bool = con.query_row(
            r#"select exists(
                   select 1
                   from image_assets asset
                   where asset.stale=0
                     and not exists(
                         select 1 from image_exact_groups exact
                         where exact.asset_id=asset.id
                     )
                   limit 1
               )"#,
            [],
            |row| row.get(0),
        )?;
        if report.indexed_files > 0
            || report.stale_files > 0
            || report.error_files > 0
            || missing_exact_groups
        {
            refresh_exact_groups(&mut con)?;
        }
        trace_timing(started, "build_index:manifest_stage_done");
    }
    if options.stage.wants_embeddings() {
        // Advance the durable content generation before the first vector
        // write. If embedding maintenance is interrupted after replacing a
        // non-final row, a subsequent process must reject the old HNSW rather
        // than accepting it because row counts and maximum rowids match.
        mark_image_index_content_changed(&con)?;
        con.execute("delete from image_index_overview_cache", [])?;
        trace_timing(started, "build_index:embedding_stage_start");
        build_embedding_stage(&mut con, &options, &required_embeddings, &mut report)?;
        trace_timing(started, "build_index:embedding_stage_done");
    }
    if options.stage.wants_manifest() || options.stage.wants_embeddings() {
        if report.indexed_files > 0 || report.stale_files > 0 || report.error_files > 0 {
            mark_image_index_content_changed(&con)?;
        }
        refresh_overview_cache(&mut con)?;
        trace_timing(started, "build_index:overview_cache_done");
    }
    report.quality = asset_quality_stats(&con)?;
    trace_timing(started, "build_index:asset_quality_stats_done");
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    trace_timing(started, "build_index:done");
    Ok(report)
}

pub fn status(root: &Path, account: &str) -> anyhow::Result<StatusReport> {
    status_with_manifest(root, account, None)
}

pub fn status_with_manifest(
    root: &Path,
    account: &str,
    manifest_path: Option<&Path>,
) -> anyhow::Result<StatusReport> {
    let manifest = manifest_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_manifest_path(root, account));
    let index_root = default_index_root(root, account);
    let model_status = model_status(&BuildOptions {
        root: root.to_path_buf(),
        account: account.to_string(),
        asset_roots: Vec::new(),
        manifest_path: None,
        max_files: DEFAULT_MAX_FILES,
        force: false,
        pipeline: "full".to_string(),
        stage: BuildStage::All,
        model_dir: None,
        sscd_model_dir: None,
        clip_model: "mobileclip2-s2".to_string(),
        sscd_model: "sscd_disc_mixup".to_string(),
        execution_provider: "auto".to_string(),
        backend: "manifest-sqlite".to_string(),
        clip_batch_size: 0,
        manifest_workers: 1,
        manifest_mode: ManifestMode::Full,
    });
    if !manifest.is_file() {
        return Ok(StatusReport {
            account: account.to_string(),
            index_root: index_root.display().to_string(),
            manifest: manifest.display().to_string(),
            manifest_exists: false,
            images: 0,
            stale_images: 0,
            errors: 0,
            exact_hashes: 0,
            perceptual_hashes: 0,
            embeddings: EmbeddingStats::default(),
            quality: AssetQualityStats::default(),
            model_status,
        });
    }
    let con = Connection::open(&manifest)?;
    init_schema(&con)?;
    let embeddings = embedding_stats(&con)?;
    Ok(StatusReport {
        account: account.to_string(),
        index_root: index_root.display().to_string(),
        manifest: manifest.display().to_string(),
        manifest_exists: true,
        images: count_i64(&con, "select count(*) from image_assets where stale=0")?,
        stale_images: count_i64(&con, "select count(*) from image_assets where stale!=0")?,
        errors: count_i64(&con, "select count(*) from image_assets where error is not null and error!=''")?,
        exact_hashes: count_i64(&con, "select count(distinct sha256_hex) from image_assets where stale=0 and sha256_hex!=''")?,
        perceptual_hashes: count_i64(&con, "select count(*) from image_assets where stale=0 and phash_hex is not null and phash_hex!=''")?,
        embeddings,
        quality: asset_quality_stats(&con)?,
        model_status,
    })
}

pub fn import_embeddings(
    options: ImportEmbeddingsOptions,
) -> anyhow::Result<ImportEmbeddingsReport> {
    let target_manifest = options
        .target_manifest
        .clone()
        .unwrap_or_else(|| default_manifest_path(&options.root, &options.account));
    if !options.source_manifest.is_file() {
        anyhow::bail!(
            "source image index manifest is missing: {}",
            options.source_manifest.display()
        );
    }
    if !target_manifest.is_file() {
        anyhow::bail!(
            "target image index manifest is missing: {}",
            target_manifest.display()
        );
    }

    let mut con = Connection::open(&target_manifest)?;
    init_schema(&con)?;
    con.execute(
        "attach database ?1 as source_manifest",
        [options.source_manifest.to_string_lossy().to_string()],
    )?;
    ensure_attached_image_index_tables(&con)?;

    let source_has_sketch =
        attached_column_exists(&con, "source_manifest", "image_embeddings", "sketch64_hex")?;
    let source_has_bucket =
        attached_column_exists(&con, "source_manifest", "image_embeddings", "bucket12")?;
    let source_has_updated =
        attached_column_exists(&con, "source_manifest", "image_embeddings", "updated_at")?;

    let source_rows = count_i64(
        &con,
        "select count(*) from source_manifest.image_embeddings",
    )?;
    let target_active_images = count_i64(&con, "select count(*) from image_assets where stale=0")?;
    let matching_rows = count_i64(&con, IMPORT_EMBEDDINGS_MATCHING_COUNT_SQL)?;
    let existing_matching_rows = count_i64(&con, IMPORT_EMBEDDINGS_EXISTING_COUNT_SQL)?;

    let sketch_expr = if source_has_sketch {
        "case when length(coalesce(e.sketch64_hex, ''))=16 then e.sketch64_hex else '' end"
    } else {
        "''"
    };
    let bucket_expr = if source_has_bucket {
        "e.bucket12"
    } else {
        "null"
    };
    let updated_expr = if source_has_updated {
        "case when coalesce(e.updated_at, '')='' then datetime('now') else e.updated_at end"
    } else {
        "datetime('now')"
    };
    let conflict_clause = if options.force {
        r#"
        on conflict(path, kind, model) do update set
            dim=excluded.dim,
            vec=excluded.vec,
            sketch64_hex=excluded.sketch64_hex,
            bucket12=excluded.bucket12,
            updated_at=excluded.updated_at
        "#
    } else {
        "on conflict(path, kind, model) do nothing"
    };
    let import_sql = format!(
        r#"
        insert into image_embeddings
            (path, kind, model, dim, vec, sketch64_hex, bucket12, updated_at)
        select
            e.path,
            e.kind,
            e.model,
            e.dim,
            e.vec,
            {sketch_expr},
            {bucket_expr},
            {updated_expr}
        {IMPORT_EMBEDDINGS_MATCHING_FROM_SQL}
        {conflict_clause}
        "#
    );

    let tx = con.transaction()?;
    let imported_rows = tx.execute(&import_sql, [])?;
    if imported_rows > 0 {
        mark_image_index_content_changed(&tx)?;
    }
    tx.commit()?;
    let backfilled_embeddings = backfill_embedding_metadata(&mut con)?;
    let embeddings = embedding_stats(&con)?;
    let skipped_existing_rows = if options.force {
        0
    } else {
        existing_matching_rows.min(matching_rows)
    };
    let _ = con.execute("detach database source_manifest", []);
    Ok(ImportEmbeddingsReport {
        account: options.account,
        source_manifest: options.source_manifest.display().to_string(),
        target_manifest: target_manifest.display().to_string(),
        force: options.force,
        source_rows,
        target_active_images,
        matching_rows,
        imported_rows,
        skipped_existing_rows,
        backfilled_embeddings,
        embeddings,
    })
}

const IMPORT_EMBEDDINGS_MATCHING_FROM_SQL: &str = r#"
from source_manifest.image_embeddings e
join source_manifest.image_assets source_asset
  on source_asset.path=e.path
join image_assets target_asset
  on target_asset.path=e.path
where source_asset.stale=0
  and target_asset.stale=0
  and (target_asset.error is null or target_asset.error='')
  and e.dim > 0
  and length(e.vec)=e.dim * 4
  and (
      coalesce(source_asset.sha256_hex, '')=''
      or coalesce(target_asset.sha256_hex, '')=''
      or source_asset.sha256_hex=target_asset.sha256_hex
  )
  and (
      source_asset.file_size=0
      or target_asset.file_size=0
      or source_asset.file_size=target_asset.file_size
  )
"#;

const IMPORT_EMBEDDINGS_MATCHING_COUNT_SQL: &str = r#"
select count(*)
from source_manifest.image_embeddings e
join source_manifest.image_assets source_asset
  on source_asset.path=e.path
join image_assets target_asset
  on target_asset.path=e.path
where source_asset.stale=0
  and target_asset.stale=0
  and (target_asset.error is null or target_asset.error='')
  and e.dim > 0
  and length(e.vec)=e.dim * 4
  and (
      coalesce(source_asset.sha256_hex, '')=''
      or coalesce(target_asset.sha256_hex, '')=''
      or source_asset.sha256_hex=target_asset.sha256_hex
  )
  and (
      source_asset.file_size=0
      or target_asset.file_size=0
      or source_asset.file_size=target_asset.file_size
  )
"#;

const IMPORT_EMBEDDINGS_EXISTING_COUNT_SQL: &str = r#"
select count(*)
from source_manifest.image_embeddings e
join source_manifest.image_assets source_asset
  on source_asset.path=e.path
join image_assets target_asset
  on target_asset.path=e.path
join image_embeddings existing
  on existing.path=e.path
 and existing.kind=e.kind
 and existing.model=e.model
where source_asset.stale=0
  and target_asset.stale=0
  and (target_asset.error is null or target_asset.error='')
  and e.dim > 0
  and length(e.vec)=e.dim * 4
  and (
      coalesce(source_asset.sha256_hex, '')=''
      or coalesce(target_asset.sha256_hex, '')=''
      or source_asset.sha256_hex=target_asset.sha256_hex
  )
  and (
      source_asset.file_size=0
      or target_asset.file_size=0
      or source_asset.file_size=target_asset.file_size
  )
"#;

pub fn benchmark_manifest(
    options: ManifestBenchmarkOptions,
) -> anyhow::Result<ManifestBenchmarkReport> {
    let started = Instant::now();
    let options = options.normalized();
    if options.mode.requires_turbojpeg_backend() {
        ensure_turbojpeg_benchmark_backend()?;
    }
    let build_options = BuildOptions {
        root: options.root.clone(),
        account: options.account.clone(),
        asset_roots: options.asset_roots.clone(),
        manifest_path: None,
        max_files: options.max_files,
        force: false,
        pipeline: "full".to_string(),
        stage: BuildStage::Manifest,
        model_dir: None,
        sscd_model_dir: None,
        clip_model: "mobileclip2-s2".to_string(),
        sscd_model: "sscd_disc_mixup".to_string(),
        execution_provider: "cpu".to_string(),
        backend: "manifest-sqlite".to_string(),
        clip_batch_size: 0,
        manifest_workers: 1,
        manifest_mode: ManifestMode::Full,
    }
    .normalized();
    let roots = scan_roots(&build_options);
    let sqlite_path = if options.mode == ManifestBenchmarkMode::SqliteBatch {
        Some(options.sqlite_path.clone().unwrap_or_else(|| {
            default_index_root(&options.root, &options.account).join("manifest-bench.sqlite")
        }))
    } else {
        None
    };
    let mut report = ManifestBenchmarkReport {
        account: options.account.clone(),
        mode: options.mode.as_str().to_string(),
        enum_backend: benchmark_enum_backend().to_string(),
        roots: roots.iter().map(|p| p.display().to_string()).collect(),
        max_files: options.max_files,
        header_bytes: options.header_bytes,
        sqlite_batch_size: options.batch_size,
        sqlite_path: sqlite_path.as_ref().map(|p| p.display().to_string()),
        root_stats: Vec::new(),
        directories_seen: 0,
        files_seen: 0,
        supported_files: 0,
        processed_files: 0,
        skipped_files: 0,
        error_files: 0,
        decode_errors: 0,
        open_errors: 0,
        fileinfo_errors: 0,
        read_errors: 0,
        walk_errors: 0,
        find_first_count: 0,
        find_next_count: 0,
        create_file_count: 0,
        fileinfo_count: 0,
        read_count: 0,
        decode_count: 0,
        sqlite_insert_count: 0,
        bytes_read: 0,
        elapsed_ms: 0,
        timing: ManifestBenchmarkTiming::default(),
        decode_profile: None,
    };
    let mut sqlite = match sqlite_path {
        Some(path) => Some(BenchmarkSqliteWriter::open(
            path,
            options.batch_size,
            &mut report,
        )?),
        None => None,
    };
    let mut decode_profile = if matches!(
        options.mode,
        ManifestBenchmarkMode::DecodeProfile
            | ManifestBenchmarkMode::JpegLumaDecodeProfile
            | ManifestBenchmarkMode::TurboJpegDecodeProfile
    ) {
        Some(BenchmarkDecodeProfile::default())
    } else {
        None
    };
    for root in &roots {
        if report.processed_files >= options.max_files {
            break;
        }
        let mut root_report = ManifestBenchmarkRootReport {
            root: root.display().to_string(),
            ..ManifestBenchmarkRootReport::default()
        };
        let root_started = Instant::now();
        benchmark_walk_root(
            root,
            &options,
            &mut report,
            &mut root_report,
            sqlite.as_mut(),
            decode_profile.as_mut(),
        )?;
        if let Some(writer) = sqlite.as_mut() {
            writer.flush(&mut report, &mut root_report)?;
        }
        root_report.elapsed_ms = elapsed_ms_u64(root_started);
        report.root_stats.push(root_report);
    }
    if let Some(writer) = sqlite.as_mut() {
        writer.flush_without_root(&mut report)?;
    }
    if let Some(profile) = decode_profile {
        report.decode_profile = Some(profile.finish());
    }
    report.elapsed_ms = elapsed_ms_u64(started);
    Ok(report)
}

pub fn query_image(options: QueryImageOptions) -> anyhow::Result<QueryReport> {
    query_image_inner(options, None)
}

pub fn query_image_cached(
    options: QueryImageOptions,
    runtime_cache: &mut QueryRuntimeCache,
) -> anyhow::Result<QueryReport> {
    query_image_inner(options, Some(runtime_cache))
}

fn query_image_inner(
    options: QueryImageOptions,
    mut runtime_cache: Option<&mut QueryRuntimeCache>,
) -> anyhow::Result<QueryReport> {
    let limit = normalize_query_limit(options.limit);
    let query_strategy = options.query_strategy;
    let threshold = if options.hamming_threshold == 0 {
        DEFAULT_HAMMING_THRESHOLD
    } else {
        options.hamming_threshold
    };
    let (mut fingerprint, mut query_image) = if options.mode.wants_exact() {
        (exact_query_fingerprint(&options.image)?, None)
    } else {
        fingerprint_image_for_query(&options.image, &options.mode)?
    };
    let con = open_existing_manifest(
        &options.root,
        &options.account,
        options.manifest_path.as_deref(),
    )?;
    let mut results = Vec::new();
    if options.mode.wants_exact() {
        let exact = exact_matches(&con, &fingerprint.sha256, limit)?;
        hydrate_query_fingerprint_from_matches(&mut fingerprint, &exact);
        for item in exact {
            push_result(&mut results, "exact", 1.0, None, item, limit);
        }
    }
    let mut unavailable = Vec::new();
    if options.mode.wants_copy() && !result_buffer_excludes_kind(&results, limit, "copy_sscd") {
        let sscd_options = BuildOptions {
            root: options.root.clone(),
            account: options.account.clone(),
            asset_roots: Vec::new(),
            manifest_path: options.manifest_path.clone(),
            max_files: DEFAULT_MAX_FILES,
            force: false,
            pipeline: "full".to_string(),
            stage: BuildStage::All,
            model_dir: None,
            sscd_model_dir: options.sscd_model_dir.clone(),
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: options.sscd_model.clone(),
            execution_provider: options.execution_provider.clone(),
            backend: "manifest-sqlite".to_string(),
            clip_batch_size: 0,
            manifest_workers: 1,
            manifest_mode: ManifestMode::Full,
        };
        if let Some(cache) = runtime_cache.as_mut() {
            match cache.sscd_runtime(&sscd_options)? {
                Some(runtime) => {
                    ensure_query_fingerprint(
                        &options.image,
                        &mut fingerprint,
                        &mut query_image,
                        true,
                    )?;
                    append_copy_results(
                        &con,
                        &mut results,
                        runtime,
                        query_image.as_ref(),
                        &options.image,
                        query_strategy,
                        limit,
                    )?;
                }
                None => unavailable.push(sscd_unavailable_signal(
                    &options.root,
                    options.sscd_model_dir.as_ref(),
                )),
            }
        } else {
            match load_sscd_runtime(&sscd_options)? {
                Some(runtime) => {
                    ensure_query_fingerprint(
                        &options.image,
                        &mut fingerprint,
                        &mut query_image,
                        true,
                    )?;
                    append_copy_results(
                        &con,
                        &mut results,
                        &runtime,
                        query_image.as_ref(),
                        &options.image,
                        query_strategy,
                        limit,
                    )?;
                }
                None => unavailable.push(sscd_unavailable_signal(
                    &options.root,
                    options.sscd_model_dir.as_ref(),
                )),
            }
        }
    }
    if options.mode.wants_near() && !result_buffer_excludes_kind(&results, limit, "near_hash") {
        ensure_query_fingerprint(&options.image, &mut fingerprint, &mut query_image, false)?;
        for (item, distance) in near_hash_matches(
            &con,
            &fingerprint.phash,
            &fingerprint.phash_algo,
            threshold,
            expanded_query_limit(limit),
        )? {
            let score = 1.0 - (distance as f64 / 64.0);
            push_result(
                &mut results,
                "near_hash",
                score,
                Some(distance),
                item,
                limit,
            );
        }
    }
    if options.mode.wants_tile() && !result_buffer_excludes_kind(&results, limit, "tile_hash") {
        ensure_query_fingerprint(&options.image, &mut fingerprint, &mut query_image, false)?;
        let tile_threshold = match options.mode {
            QueryMode::Strict => TILE_STRICT_HAMMING_THRESHOLD,
            _ => TILE_DEFAULT_HAMMING_THRESHOLD,
        };
        for (item, distance) in tile_hash_matches(
            &con,
            &fingerprint.tile_hashes,
            tile_threshold,
            expanded_query_limit(limit),
        )? {
            let matched = item.matched_tile_count.unwrap_or(1) as f64;
            let score = (1.0 - (distance as f64 / 64.0)) + (matched.min(8.0) * 0.01);
            push_result(
                &mut results,
                "tile_hash",
                score,
                Some(distance),
                item,
                limit,
            );
        }
    }
    if options.mode.wants_semantic()
        && !result_buffer_excludes_kind(&results, limit, "semantic_clip")
    {
        let clip_options = BuildOptions {
            root: options.root.clone(),
            account: options.account.clone(),
            asset_roots: Vec::new(),
            manifest_path: options.manifest_path.clone(),
            max_files: DEFAULT_MAX_FILES,
            force: false,
            pipeline: "full".to_string(),
            stage: BuildStage::All,
            model_dir: options.model_dir.clone(),
            sscd_model_dir: None,
            clip_model: options.clip_model.clone(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: options.execution_provider.clone(),
            backend: "manifest-sqlite".to_string(),
            clip_batch_size: 0,
            manifest_workers: 1,
            manifest_mode: ManifestMode::Full,
        };
        if let Some(cache) = runtime_cache.as_mut() {
            match cache.clip_vision_runtime(&clip_options)? {
                Some(runtime) => {
                    ensure_query_fingerprint(
                        &options.image,
                        &mut fingerprint,
                        &mut query_image,
                        true,
                    )?;
                    append_semantic_image_results(
                        &con,
                        &mut results,
                        runtime,
                        query_image.as_ref(),
                        &options.image,
                        query_strategy,
                        limit,
                    )?;
                }
                None => unavailable.push(clip_unavailable_signal(
                    &options.root,
                    options.model_dir.as_ref(),
                )),
            }
        } else {
            match load_clip_vision_runtime(&clip_options)? {
                Some(runtime) => {
                    ensure_query_fingerprint(
                        &options.image,
                        &mut fingerprint,
                        &mut query_image,
                        true,
                    )?;
                    append_semantic_image_results(
                        &con,
                        &mut results,
                        &runtime,
                        query_image.as_ref(),
                        &options.image,
                        query_strategy,
                        limit,
                    )?;
                }
                None => unavailable.push(clip_unavailable_signal(
                    &options.root,
                    options.model_dir.as_ref(),
                )),
            }
        }
    }
    results.sort_by(query_result_sort_cmp);
    results.truncate(limit);
    let query = QueryDescriptor {
        path: options.image.display().to_string(),
        sha256: fingerprint.sha256.clone(),
        phash: fingerprint.phash.clone(),
        phash_algo: fingerprint.phash_algo.clone(),
        width: fingerprint.width,
        height: fingerprint.height,
    };
    Ok(QueryReport {
        account: options.account,
        query,
        mode: format!("{:?}", options.mode).to_ascii_lowercase(),
        query_strategy: query_strategy.as_str().to_string(),
        results,
        unavailable,
    })
}

pub fn query_text(options: QueryTextOptions) -> anyhow::Result<TextQueryReport> {
    query_text_inner(options, None)
}

pub fn query_text_cached(
    options: QueryTextOptions,
    runtime_cache: &mut QueryRuntimeCache,
) -> anyhow::Result<TextQueryReport> {
    query_text_inner(options, Some(runtime_cache))
}

fn query_text_inner(
    options: QueryTextOptions,
    runtime_cache: Option<&mut QueryRuntimeCache>,
) -> anyhow::Result<TextQueryReport> {
    let limit = normalize_query_limit(options.limit);
    let query_strategy = options.query_strategy;
    let clip_options = BuildOptions {
        root: options.root.clone(),
        account: options.account.clone(),
        asset_roots: Vec::new(),
        manifest_path: options.manifest_path.clone(),
        max_files: DEFAULT_MAX_FILES,
        force: false,
        pipeline: "full".to_string(),
        stage: BuildStage::All,
        model_dir: options.model_dir.clone(),
        sscd_model_dir: None,
        clip_model: options.clip_model.clone(),
        sscd_model: "sscd_disc_mixup".to_string(),
        execution_provider: options.execution_provider.clone(),
        backend: "manifest-sqlite".to_string(),
        clip_batch_size: 0,
        manifest_workers: 1,
        manifest_mode: ManifestMode::Full,
    };
    if let Some(cache) = runtime_cache {
        if let Some(runtime) = cache.clip_text_runtime(&clip_options)? {
            let con = open_existing_manifest(
                &options.root,
                &options.account,
                options.manifest_path.as_deref(),
            )?;
            let results =
                semantic_text_results(&con, runtime, &options.text, query_strategy, limit)?;
            return Ok(TextQueryReport {
                account: options.account,
                text: options.text,
                query_strategy: query_strategy.as_str().to_string(),
                limit,
                results,
                unavailable: Vec::new(),
            });
        }
    } else if let Some(runtime) = load_clip_text_runtime(&clip_options)? {
        let con = open_existing_manifest(
            &options.root,
            &options.account,
            options.manifest_path.as_deref(),
        )?;
        let results = semantic_text_results(&con, &runtime, &options.text, query_strategy, limit)?;
        return Ok(TextQueryReport {
            account: options.account,
            text: options.text,
            query_strategy: query_strategy.as_str().to_string(),
            limit,
            results,
            unavailable: Vec::new(),
        });
    }
    let unavailable = vec![clip_unavailable_signal(
        &options.root,
        options.model_dir.as_ref(),
    )];
    Ok(TextQueryReport {
        account: options.account,
        text: options.text,
        query_strategy: query_strategy.as_str().to_string(),
        limit,
        results: Vec::new(),
        unavailable,
    })
}

pub fn status_json(root: &Path, account: &str) -> anyhow::Result<String> {
    Ok(serde_json::to_string(&status(root, account)?)?)
}

pub fn similar_asset_json(
    root: &Path,
    account: &str,
    asset: &str,
    mode: QueryMode,
    limit: usize,
) -> anyhow::Result<String> {
    similar_asset_json_with_strategy(root, account, asset, mode, QueryStrategy::Exact, limit)
}

pub fn similar_asset_json_with_strategy(
    root: &Path,
    account: &str,
    asset: &str,
    mode: QueryMode,
    query_strategy: QueryStrategy,
    limit: usize,
) -> anyhow::Result<String> {
    let image = decode_asset_path(root, asset)?;
    let report = query_image(QueryImageOptions {
        root: root.to_path_buf(),
        account: account.to_string(),
        manifest_path: None,
        image,
        mode,
        query_strategy,
        limit,
        hamming_threshold: DEFAULT_HAMMING_THRESHOLD,
        model_dir: default_clip_model_dir(root),
        sscd_model_dir: default_sscd_model_dir(root),
        clip_model: "mobileclip2-s2".to_string(),
        sscd_model: "sscd_disc_mixup".to_string(),
        execution_provider: "auto".to_string(),
    })?;
    Ok(serde_json::to_string(&report)?)
}

/// Searches for an image that is already present in the manifest.
///
/// Unlike `similar_asset_json_with_strategy`, this path never opens or decodes
/// the source image and never initializes an inference runtime.  The manifest
/// already contains every query signal needed by an indexed asset: SHA-256,
/// pHash/tile hashes, and the SSCD/CLIP vectors.  Keeping this as a separate API
/// also makes it difficult for the web service to accidentally send indexed
/// assets through the upload-image inference actor again.
pub fn similar_indexed_asset_json_with_strategy(
    root: &Path,
    account: &str,
    asset_id: i64,
    mode: QueryMode,
    query_strategy: QueryStrategy,
    limit: usize,
) -> anyhow::Result<String> {
    let report = query_indexed_asset(root, account, asset_id, mode, query_strategy, limit)?;
    Ok(serde_json::to_string(&report)?)
}

/// Resolves the SSCD >= 0.98 same-image set for provenance aggregation.  This
/// deliberately uses the web latency-bounded vector shortlist; SHA-256 copies
/// are expanded separately by the occurrence query.
pub fn same_image_asset_ids(root: &Path, account: &str, asset_id: i64) -> anyhow::Result<Vec<i64>> {
    let report = query_indexed_asset(
        root,
        account,
        asset_id,
        QueryMode::Copy,
        QueryStrategy::Fast,
        MAX_QUERY_RESULTS,
    )?;
    let con = open_existing_manifest(root, account, None)?;
    let mut stmt = con.prepare_cached("select id from image_assets where path=?1 and stale=0")?;
    let mut ids = vec![asset_id];
    for result in report.results {
        if result.score < SSCD_SAME_IMAGE_THRESHOLD {
            continue;
        }
        if let Some(id) = stmt
            .query_row([result.path.as_str()], |row| row.get::<_, i64>(0))
            .optional()?
        {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

fn query_indexed_asset(
    root: &Path,
    account: &str,
    asset_id: i64,
    mode: QueryMode,
    query_strategy: QueryStrategy,
    limit: usize,
) -> anyhow::Result<QueryReport> {
    let con = open_existing_manifest(root, account, None)?;
    let indexed = con
        .query_row(
            r#"select path, sha256_hex, phash_hex, phash_algo, width, height,
                      blur_score, blur_algo, quality_flags, source_class
               from image_assets where id=?1 and stale=0"#,
            [asset_id],
            image_from_row,
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("image asset {asset_id} was not found"))?;
    let path_text = indexed.path.to_string_lossy().to_string();
    let limit = normalize_query_limit(limit);
    let mut results = Vec::new();
    let mut unavailable = Vec::new();

    if mode.wants_exact() && !indexed.sha256.is_empty() {
        for item in exact_matches(&con, &indexed.sha256, limit)? {
            push_result(&mut results, "exact", 1.0, None, item, limit);
        }
    }

    if mode.wants_copy() && !result_buffer_excludes_kind(&results, limit, "copy_sscd") {
        if let Some((model, vector)) = indexed_embedding(&con, &path_text, "sscd")? {
            for (item, score) in semantic_matches(
                &con,
                "sscd",
                &model,
                &vector,
                query_strategy,
                expanded_query_limit(limit),
            )? {
                push_result(
                    &mut results,
                    sscd_match_kind(score),
                    score,
                    None,
                    item,
                    limit,
                );
            }
        } else {
            unavailable.push(UnavailableSignal {
                signal: "copy_descriptor:sscd_vec".to_string(),
                reason: "该索引图片没有可用的 SSCD 向量".to_string(),
            });
        }
    }

    if mode.wants_near() && !result_buffer_excludes_kind(&results, limit, "near_hash") {
        let phash = indexed.phash.as_deref().unwrap_or("");
        let phash_algo = indexed.phash_algo.as_deref().unwrap_or("");
        if phash.len() == 16 && !phash_algo.is_empty() {
            for (item, distance) in near_hash_matches(
                &con,
                phash,
                phash_algo,
                DEFAULT_HAMMING_THRESHOLD,
                expanded_query_limit(limit),
            )? {
                let score = 1.0 - (distance as f64 / 64.0);
                push_result(
                    &mut results,
                    "near_hash",
                    score,
                    Some(distance),
                    item,
                    limit,
                );
            }
        } else {
            unavailable.push(UnavailableSignal {
                signal: "pixel_fingerprint:phash".to_string(),
                reason: "该索引图片没有可用的 pHash".to_string(),
            });
        }
    }

    if mode.wants_tile() && !result_buffer_excludes_kind(&results, limit, "tile_hash") {
        let query_tiles = indexed_tile_hashes(&con, asset_id)?;
        if query_tiles.is_empty() {
            unavailable.push(UnavailableSignal {
                signal: "pixel_fingerprint:tile_hash".to_string(),
                reason: "该索引图片没有可用的局部结构指纹".to_string(),
            });
        } else {
            let threshold = if mode == QueryMode::Strict {
                TILE_STRICT_HAMMING_THRESHOLD
            } else {
                TILE_DEFAULT_HAMMING_THRESHOLD
            };
            for (item, distance) in
                tile_hash_matches(&con, &query_tiles, threshold, expanded_query_limit(limit))?
            {
                let matched = item.matched_tile_count.unwrap_or(1) as f64;
                let score = (1.0 - (distance as f64 / 64.0)) + (matched.min(8.0) * 0.01);
                push_result(
                    &mut results,
                    "tile_hash",
                    score,
                    Some(distance),
                    item,
                    limit,
                );
            }
        }
    }

    if mode.wants_semantic() && !result_buffer_excludes_kind(&results, limit, "semantic_clip") {
        if let Some((model, vector)) = indexed_embedding(&con, &path_text, "clip")? {
            for (item, score) in semantic_matches(
                &con,
                "clip",
                &model,
                &vector,
                query_strategy,
                expanded_query_limit(limit),
            )? {
                push_result(&mut results, "semantic_clip", score, None, item, limit);
            }
        } else {
            unavailable.push(UnavailableSignal {
                signal: "semantic_descriptor:clip_vec".to_string(),
                reason: "该索引图片没有可用的 CLIP 向量".to_string(),
            });
        }
    }

    results.sort_by(query_result_sort_cmp);
    results.truncate(limit);
    Ok(QueryReport {
        account: account.to_string(),
        query: QueryDescriptor {
            path: path_text,
            sha256: indexed.sha256,
            phash: indexed.phash.unwrap_or_default(),
            phash_algo: indexed.phash_algo.unwrap_or_default(),
            width: indexed.width.unwrap_or(0),
            height: indexed.height.unwrap_or(0),
        },
        mode: format!("{mode:?}").to_ascii_lowercase(),
        query_strategy: query_strategy.as_str().to_string(),
        results,
        unavailable,
    })
}

fn indexed_embedding(
    con: &Connection,
    path: &str,
    kind: &str,
) -> anyhow::Result<Option<(String, Vec<f32>)>> {
    let mut stmt = con.prepare_cached(
        r#"select model, dim, vec
           from image_embeddings
           where path=?1 and kind=?2
           order by updated_at desc, model
        "#,
    )?;
    let mut rows = stmt.query(params![path, kind])?;
    while let Some(row) = rows.next()? {
        let model = row.get::<_, String>(0)?;
        let dim = row.get::<_, i64>(1)?;
        let bytes = row.get::<_, Vec<u8>>(2)?;
        if dim <= 0 || bytes.len() != dim as usize * 4 {
            continue;
        }
        let vector = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        return Ok(Some((model, vector)));
    }
    Ok(None)
}

fn indexed_tile_hashes(con: &Connection, asset_id: i64) -> anyhow::Result<Vec<TileHash>> {
    let mut stmt = con.prepare_cached(
        r#"select tile_algo, tile_set, tile_index, x0, y0, x1, y1, hash_hex
           from image_asset_tile_hashes
           where asset_id=?1
           order by tile_algo, tile_set, tile_index"#,
    )?;
    let rows = stmt.query_map([asset_id], |row| {
        Ok(TileHash {
            tile_algo: row.get(0)?,
            tile_set: row.get(1)?,
            tile_index: row.get(2)?,
            x0: row.get(3)?,
            y0: row.get(4)?,
            x1: row.get(5)?,
            y1: row.get(6)?,
            hash_hex: row.get(7)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn search_text_json(
    root: &Path,
    account: &str,
    text: &str,
    limit: usize,
) -> anyhow::Result<String> {
    search_text_json_with_strategy(root, account, text, QueryStrategy::Exact, limit)
}

pub fn search_text_json_with_strategy(
    root: &Path,
    account: &str,
    text: &str,
    query_strategy: QueryStrategy,
    limit: usize,
) -> anyhow::Result<String> {
    let report = query_text(QueryTextOptions {
        root: root.to_path_buf(),
        account: account.to_string(),
        manifest_path: None,
        text: text.to_string(),
        query_strategy,
        limit,
        model_dir: default_clip_model_dir(root),
        clip_model: "mobileclip2-s2".to_string(),
        execution_provider: "auto".to_string(),
    })?;
    Ok(serde_json::to_string(&report)?)
}

pub fn similar_asset_json_cached(
    root: &Path,
    account: &str,
    asset: &str,
    mode: QueryMode,
    limit: usize,
    runtime_cache: &mut QueryRuntimeCache,
) -> anyhow::Result<String> {
    similar_asset_json_cached_with_strategy(
        root,
        account,
        asset,
        mode,
        QueryStrategy::Exact,
        limit,
        runtime_cache,
    )
}

pub fn similar_asset_json_cached_with_strategy(
    root: &Path,
    account: &str,
    asset: &str,
    mode: QueryMode,
    query_strategy: QueryStrategy,
    limit: usize,
    runtime_cache: &mut QueryRuntimeCache,
) -> anyhow::Result<String> {
    let image = decode_asset_path(root, asset)?;
    let report = query_image_cached(
        QueryImageOptions {
            root: root.to_path_buf(),
            account: account.to_string(),
            manifest_path: None,
            image,
            mode,
            query_strategy,
            limit,
            hamming_threshold: DEFAULT_HAMMING_THRESHOLD,
            model_dir: default_clip_model_dir(root),
            sscd_model_dir: default_sscd_model_dir(root),
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "auto".to_string(),
        },
        runtime_cache,
    )?;
    Ok(serde_json::to_string(&report)?)
}

pub fn search_text_json_cached(
    root: &Path,
    account: &str,
    text: &str,
    limit: usize,
    runtime_cache: &mut QueryRuntimeCache,
) -> anyhow::Result<String> {
    search_text_json_cached_with_strategy(
        root,
        account,
        text,
        QueryStrategy::Exact,
        limit,
        runtime_cache,
    )
}

pub fn search_text_json_cached_with_strategy(
    root: &Path,
    account: &str,
    text: &str,
    query_strategy: QueryStrategy,
    limit: usize,
    runtime_cache: &mut QueryRuntimeCache,
) -> anyhow::Result<String> {
    let report = query_text_cached(
        QueryTextOptions {
            root: root.to_path_buf(),
            account: account.to_string(),
            manifest_path: None,
            text: text.to_string(),
            query_strategy,
            limit,
            model_dir: default_clip_model_dir(root),
            clip_model: "mobileclip2-s2".to_string(),
            execution_provider: "auto".to_string(),
        },
        runtime_cache,
    )?;
    Ok(serde_json::to_string(&report)?)
}

pub fn query_uploaded_image_json_cached_with_strategy(
    root: &Path,
    account: &str,
    image: &Path,
    mode: QueryMode,
    query_strategy: QueryStrategy,
    limit: usize,
    runtime_cache: &mut QueryRuntimeCache,
) -> anyhow::Result<String> {
    let report = query_image_cached(
        QueryImageOptions {
            root: root.to_path_buf(),
            account: account.to_string(),
            manifest_path: None,
            image: image.to_path_buf(),
            mode,
            query_strategy,
            limit,
            hamming_threshold: DEFAULT_HAMMING_THRESHOLD,
            model_dir: default_clip_model_dir(root),
            sscd_model_dir: default_sscd_model_dir(root),
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "auto".to_string(),
        },
        runtime_cache,
    )?;
    Ok(serde_json::to_string(&report)?)
}

/// Initializes web-facing materializations in the generated manifest. The source
/// QQ databases are never opened or modified by this operation.
pub fn prepare_web_manifest(root: &Path, account: &str) -> anyhow::Result<()> {
    let manifest = default_manifest_path(root, account);
    prepare_web_manifest_path(&manifest)
}

pub fn prepare_web_manifest_path(manifest: &Path) -> anyhow::Result<()> {
    if !manifest.is_file() {
        return Ok(());
    }
    let mut con = Connection::open(manifest)?;
    con.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    init_schema(&con)?;
    let has_overview: bool = con.query_row(
        "select exists(select 1 from image_index_overview_cache where id=1 and schema_version=1)",
        [],
        |row| row.get(0),
    )?;
    if !has_overview {
        let active_assets: i64 = con.query_row(
            "select count(*) from image_assets where stale=0",
            [],
            |row| row.get(0),
        )?;
        let grouped_assets: i64 =
            con.query_row("select count(*) from image_exact_groups", [], |row| {
                row.get(0)
            })?;
        if grouped_assets != active_assets {
            refresh_exact_groups(&mut con)?;
        }
        refresh_overview_cache(&mut con)?;
    }
    let overview_occurrences = con
        .query_row(
            "select occurrence_row_count from image_index_overview_cache where id=1 and schema_version=1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let popularity_state = con
        .query_row(
            r#"select occurrence_row_count,occurrence_generation
               from image_popularity_cache_state
               where id=1 and schema_version=?1"#,
            [POPULARITY_CACHE_SCHEMA_VERSION],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let occurrence_generation = occurrence_content_generation(&con)?;
    if overview_occurrences.map(|occurrences| (occurrences, occurrence_generation))
        != popularity_state
    {
        refresh_popularity_cache(&mut con, overview_occurrences.unwrap_or(0))?;
    }
    Ok(())
}

/// Rebuilds exact-copy membership atomically. Assets without SHA-256 values are
/// intentionally assigned singleton groups.
pub fn refresh_exact_groups(con: &mut Connection) -> anyhow::Result<()> {
    let legacy_exact_groups = exact_groups_are_legacy_table(con)?;
    let tx = con.transaction()?;
    tx.execute_batch(
        r#"
        drop table if exists temp.image_exact_groups_next;
        create temp table image_exact_groups_next as
        with ranked as (
            select
                id as asset_id,
                case
                    when sha256_hex <> '' then 'sha256:' || sha256_hex
                    else 'asset:' || id
                end as group_key,
                row_number() over (
                    partition by case
                        when sha256_hex <> '' then 'sha256:' || sha256_hex
                        else 'asset:' || id
                    end
                    order by
                        case when error is null or error = '' then 0 else 1 end,
                        case when
                            quality_flags not like '%thumbnail%'
                            and quality_flags not like '%tiny%'
                            and quality_flags not like '%small%'
                            and quality_flags not like '%blurry%'
                        then 0 else 1 end,
                        coalesce(width, 0) * coalesce(height, 0) desc,
                        file_size desc,
                        id asc
                ) as representative_rank,
                count(*) over (
                    partition by case
                        when sha256_hex <> '' then 'sha256:' || sha256_hex
                        else 'asset:' || id
                    end
                ) as member_count,
                sum(file_size) over (
                    partition by case
                        when sha256_hex <> '' then 'sha256:' || sha256_hex
                        else 'asset:' || id
                    end
                ) as total_bytes,
                max(file_size) over (
                    partition by case
                        when sha256_hex <> '' then 'sha256:' || sha256_hex
                        else 'asset:' || id
                    end
                ) as largest_bytes
            from image_assets
            where stale = 0
        ), representatives as (
            select group_key, asset_id as representative_asset_id
            from ranked
            where representative_rank = 1
        )
        select
            ranked.asset_id as asset_id,
            ranked.group_key as group_key,
            representatives.representative_asset_id as representative_asset_id,
            ranked.member_count as member_count,
            ranked.total_bytes as total_bytes,
            max(ranked.total_bytes - ranked.largest_bytes, 0) as duplicate_bytes
        from ranked
        join representatives using(group_key);
        "#,
    )?;
    let mapping_changed: bool = tx.query_row(
        r#"select exists(
               select 1
               from image_exact_groups current
               left join image_exact_groups_next next on next.asset_id=current.asset_id
               where next.asset_id is null
                  or next.representative_asset_id<>current.representative_asset_id
               limit 1
           ) or exists(
               select 1
               from image_exact_groups_next next
               left join image_exact_groups current on current.asset_id=next.asset_id
               where current.asset_id is null
                  or current.representative_asset_id<>next.representative_asset_id
               limit 1
           )"#,
        [],
        |row| row.get(0),
    )?;
    tx.execute("delete from image_index_overview_cache", [])?;
    if mapping_changed {
        mark_image_index_content_changed(&tx)?;
        tx.execute("delete from image_popularity_cache_state", [])?;
        tx.execute("delete from image_asset_reference_counts", [])?;
        tx.execute("delete from image_exact_reference_counts", [])?;
        tx.execute("delete from image_popularity_cube", [])?;
        tx.execute("delete from image_popularity_daily_conversation", [])?;
        tx.execute("delete from image_popularity_summary_cache", [])?;
        tx.execute("delete from image_popularity_build_state", [])?;
    }
    tx.execute("delete from image_exact_group_members", [])?;
    tx.execute("delete from image_exact_group_stats", [])?;
    if legacy_exact_groups {
        tx.execute("drop table image_exact_groups", [])?;
    }
    tx.execute_batch(
        r#"insert into image_exact_group_stats(
               representative_asset_id,group_key,member_count,total_bytes,
               duplicate_bytes
           )
           select representative_asset_id,group_key,member_count,total_bytes,
                  duplicate_bytes
           from image_exact_groups_next
           where asset_id=representative_asset_id;
           insert into image_exact_group_members(
               asset_id,representative_asset_id
           )
           select asset_id,representative_asset_id
           from image_exact_groups_next;
           drop table image_exact_groups_next;"#,
    )?;
    if legacy_exact_groups {
        ensure_exact_group_compatibility_view(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

fn add_distribution_value(distribution: &mut HashMap<String, (i64, i64)>, key: String, bytes: i64) {
    let value = distribution.entry(key).or_default();
    value.0 += 1;
    value.1 += bytes;
}

fn sorted_distribution(distribution: HashMap<String, (i64, i64)>) -> Vec<Value> {
    let mut values = distribution
        .into_iter()
        .map(|(key, (count, bytes))| json!({"key": key, "count": count, "bytes": bytes}))
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right["count"]
            .as_i64()
            .cmp(&left["count"].as_i64())
            .then_with(|| left["key"].as_str().cmp(&right["key"].as_str()))
    });
    values
}

fn occurrence_coverage(con: &Connection) -> anyhow::Result<(i64, i64)> {
    Ok(con.query_row(
        "select count(*),count(distinct asset_id) from image_asset_occurrences",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

/// Materializes the expensive, manifest-wide web overview as a single JSON row.
/// This intentionally uses sequential scans and Rust-side aggregation so large
/// manifests do not spill several GROUP BY temporary tables to disk.
pub fn refresh_overview_cache(con: &mut Connection) -> anyhow::Result<()> {
    let mut formats = HashMap::new();
    let mut sources = HashMap::new();
    let mut size_counts = [(0_i64, 0_i64); 4];
    let mut quality_counts = [(0_i64, 0_i64); 6];
    let mut assets = 0_i64;
    let mut stale = 0_i64;
    let mut errors = 0_i64;

    {
        let mut stmt = con.prepare(
            "select detected_format,source_class,quality_flags,file_size,stale,error from image_assets",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let is_stale = row.get::<_, i64>(4)? != 0;
            if row
                .get::<_, Option<String>>(5)?
                .is_some_and(|error| !error.is_empty())
            {
                errors += 1;
            }
            if is_stale {
                stale += 1;
                continue;
            }
            assets += 1;
            let file_size = row.get::<_, i64>(3)?.max(0);
            let format = row.get::<_, String>(0)?;
            add_distribution_value(
                &mut formats,
                if format.is_empty() {
                    "unknown".to_string()
                } else {
                    format.to_ascii_lowercase()
                },
                file_size,
            );
            let source = row.get::<_, String>(1)?;
            add_distribution_value(
                &mut sources,
                if source.is_empty() {
                    "unknown".to_string()
                } else {
                    source
                },
                file_size,
            );
            let size_index = if file_size < 65_536 {
                0
            } else if file_size < 1_048_576 {
                1
            } else if file_size < 10_485_760 {
                2
            } else {
                3
            };
            size_counts[size_index].0 += 1;
            size_counts[size_index].1 += file_size;

            let flags = row.get::<_, String>(2)?;
            for (index, flag) in ["blurry", "small", "tiny", "thumbnail", "manifest_pending"]
                .iter()
                .enumerate()
            {
                if flags.contains(flag) {
                    quality_counts[index].0 += 1;
                    quality_counts[index].1 += file_size;
                }
            }
            if flags.is_empty() {
                quality_counts[5].0 += 1;
                quality_counts[5].1 += file_size;
            }
        }
    }

    let mut embedding_rows = 0_i64;
    let mut clip_assets = 0_i64;
    let mut sscd_assets = 0_i64;
    {
        let mut stmt = con.prepare(
            r#"select e.path,e.kind
               from image_embeddings e
               join image_assets a on a.path=e.path
               where a.stale=0
               order by e.path,e.kind,e.model"#,
        )?;
        let mut rows = stmt.query([])?;
        let mut last_clip_path = String::new();
        let mut last_sscd_path = String::new();
        while let Some(row) = rows.next()? {
            embedding_rows += 1;
            let path = row.get::<_, String>(0)?;
            match row.get::<_, String>(1)?.as_str() {
                "clip" if path != last_clip_path => {
                    clip_assets += 1;
                    last_clip_path = path;
                }
                "sscd" if path != last_sscd_path => {
                    sscd_assets += 1;
                    last_sscd_path = path;
                }
                _ => {}
            }
        }
    }

    let (occurrences, referenced_assets) = occurrence_coverage(con)?;
    let (exact_groups, duplicate_groups, duplicate_files, duplicate_bytes) = con.query_row(
        r#"select
            count(*),
            coalesce(sum(member_count>1),0),
            coalesce(sum(member_count-1),0),
            coalesce(sum(duplicate_bytes),0)
        from image_exact_groups
        where asset_id=representative_asset_id"#,
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    let size_labels = ["<64 KiB", "64 KiB–1 MiB", "1–10 MiB", "≥10 MiB"];
    let size_distribution = size_labels
        .into_iter()
        .zip(size_counts)
        .map(|(key, (count, bytes))| json!({"key": key, "count": count, "bytes": bytes}))
        .collect::<Vec<_>>();
    let quality_labels = [
        "blurry",
        "small",
        "tiny",
        "thumbnail",
        "manifest_pending",
        "clean",
    ];
    let quality_distribution = quality_labels
        .into_iter()
        .zip(quality_counts)
        .map(|(key, (count, bytes))| json!({"key": key, "count": count, "bytes": bytes}))
        .collect::<Vec<_>>();
    let refreshed_at = current_timestamp();
    let payload = json!({
        "schema_version": 1,
        "generated_at": refreshed_at,
        "health": {"assets": assets, "stale": stale, "errors": errors},
        "coverage": {
            "embedding_rows": embedding_rows,
            "clip_assets": clip_assets,
            "sscd_assets": sscd_assets,
            "referenced_assets": referenced_assets,
            "occurrences": occurrences,
        },
        "exact_copies": {
            "groups": exact_groups,
            "duplicate_groups": duplicate_groups,
            "duplicate_files": duplicate_files,
            "potential_savings_bytes": duplicate_bytes,
        },
        "distributions": {
            "format": sorted_distribution(formats),
            "source": sorted_distribution(sources),
            "quality": quality_distribution,
            "size": size_distribution,
        },
    });
    let payload_json = serde_json::to_string(&payload)?;
    con.execute(
        r#"insert into image_index_overview_cache(
                id,schema_version,payload_json,active_asset_count,grouped_asset_count,
                embedding_row_count,occurrence_row_count,refreshed_at
            ) values(1,1,?1,?2,?3,?4,?5,?6)
            on conflict(id) do update set
                schema_version=excluded.schema_version,
                payload_json=excluded.payload_json,
                active_asset_count=excluded.active_asset_count,
                grouped_asset_count=excluded.grouped_asset_count,
                embedding_row_count=excluded.embedding_row_count,
                occurrence_row_count=excluded.occurrence_row_count,
                refreshed_at=excluded.refreshed_at"#,
        params![
            payload_json,
            assets,
            active_assets_in_groups(exact_groups, duplicate_files),
            embedding_rows,
            occurrences,
            refreshed_at
        ],
    )?;
    Ok(())
}

fn active_assets_in_groups(exact_groups: i64, duplicate_files: i64) -> i64 {
    exact_groups + duplicate_files
}

/// Updates only the inexpensive provenance portion after an incremental chat
/// link. If no static overview exists yet, preparation will build it later.
pub fn refresh_overview_reference_cache(con: &mut Connection) -> anyhow::Result<()> {
    let payload_json = con
        .query_row(
            "select payload_json from image_index_overview_cache where id=1 and schema_version=1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(payload_json) = payload_json else {
        return Ok(());
    };
    let (occurrences, referenced_assets) = occurrence_coverage(con)?;
    refresh_popularity_cache(con, occurrences)?;
    let mut payload = serde_json::from_str::<Value>(&payload_json)?;
    if let Some(coverage) = payload.get_mut("coverage").and_then(Value::as_object_mut) {
        coverage.insert("occurrences".to_string(), json!(occurrences));
        coverage.insert("referenced_assets".to_string(), json!(referenced_assets));
    }
    let refreshed_at = current_timestamp();
    payload["generated_at"] = json!(refreshed_at);
    con.execute(
        "update image_index_overview_cache set payload_json=?1,occurrence_row_count=?2,refreshed_at=?3 where id=1",
        params![serde_json::to_string(&payload)?, occurrences, refreshed_at],
    )?;
    Ok(())
}

/// Materializes chat-reference popularity once after provenance maintenance.
/// Gallery requests can then walk a covering popularity index instead of
/// grouping the multi-million-row occurrence table for every page.
fn refresh_popularity_cache(con: &mut Connection, occurrences: i64) -> anyhow::Result<()> {
    let refreshed_at = current_timestamp();
    let occurrence_generation = occurrence_content_generation(con)?;
    let can_reuse_asset_counts = con
        .query_row(
            r#"select schema_version=?3
                      and occurrence_row_count=?1
                      and occurrence_generation=?2
                      and (
                          ?1=0 or exists(
                              select 1 from image_asset_reference_counts
                          )
                      )
               from image_popularity_cache_state
               where id=1"#,
            params![
                occurrences,
                occurrence_generation,
                POPULARITY_CACHE_SCHEMA_VERSION
            ],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    let can_reuse_cube: bool = con.query_row(
        r#"select exists(
               select 1 from image_popularity_build_state
               where id=1 and summary_ready=1
           )"#,
        [],
        |row| row.get(0),
    )?;
    let tx = con.transaction()?;
    tx.execute("delete from image_popularity_cache_state", [])?;
    tx.execute("delete from image_exact_reference_counts", [])?;
    if !can_reuse_asset_counts {
        tx.execute("delete from image_asset_reference_counts", [])?;
        tx.execute_batch(
            r#"
        insert into image_asset_reference_counts(asset_id, reference_count)
        select asset_id, count(*)
        from image_asset_occurrences
        group by asset_id;
        "#,
        )?;
    }
    if can_reuse_cube {
        tx.execute_batch(
            r#"insert into image_exact_reference_counts(
                   representative_asset_id,reference_count
               )
               select representative_asset_id,sum(reference_count)
               from image_popularity_cube
               group by representative_asset_id;"#,
        )?;
    } else {
        tx.execute_batch(
            r#"insert into image_exact_reference_counts(
                   representative_asset_id,reference_count
               )
               select representative_asset_id,count(*)
               from (
                   select distinct exact_group.representative_asset_id,
                          occurrence.conversation_table,occurrence.message_rowid
                   from image_asset_occurrences occurrence
                   join image_exact_groups exact_group
                     on exact_group.asset_id=occurrence.asset_id
               )
               group by representative_asset_id;"#,
        )?;
    }
    tx.execute(
        r#"insert into image_popularity_cache_state(
                id,schema_version,occurrence_row_count,occurrence_generation,
                refreshed_at
            ) values(1,?1,?2,?3,?4)"#,
        params![
            POPULARITY_CACHE_SCHEMA_VERSION,
            occurrences,
            occurrence_generation,
            refreshed_at
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn init_schema(con: &Connection) -> anyhow::Result<()> {
    configure_manifest_connection(con)?;
    con.execute_batch(
        r#"
        create table if not exists image_assets (
            id integer primary key autoincrement,
            path text not null unique,
            source_root text not null default '',
            file_size integer not null default 0,
            mtime_unix integer not null default 0,
            sha256_hex text not null default '',
            phash_hex text,
            phash_algo text not null default '',
            width integer,
            height integer,
            blur_score real,
            blur_algo text not null default '',
            quality_flags text not null default '',
            source_class text not null default '',
            detected_format text not null default '',
            has_alpha integer not null default 0,
            orientation_applied integer not null default 0,
            fingerprint_version text not null default '',
            indexed_at text not null default '',
            stale integer not null default 0,
            error text
        );
        create index if not exists image_assets_sha256_idx on image_assets(sha256_hex);
        create index if not exists image_assets_phash_idx on image_assets(phash_hex);
        create index if not exists image_assets_stale_idx on image_assets(stale);
        create table if not exists image_asset_tile_hashes (
            asset_id integer not null,
            tile_algo text not null,
            tile_set text not null,
            tile_index integer not null,
            x0 real not null,
            y0 real not null,
            x1 real not null,
            y1 real not null,
            hash_hex text not null,
            primary key(asset_id, tile_algo, tile_set, tile_index)
        );
        create index if not exists idx_tile_hash_algo_hash
            on image_asset_tile_hashes(tile_algo, hash_hex);
        create index if not exists idx_tile_hash_asset
            on image_asset_tile_hashes(asset_id);
        create table if not exists image_asset_local_features (
            asset_id integer not null,
            local_algo text not null,
            keypoint_count integer not null,
            descriptor_count integer not null,
            descriptor_blob blob,
            primary key(asset_id, local_algo)
        );
        create table if not exists image_embeddings (
            path text not null,
            kind text not null,
            model text not null,
            dim integer not null,
            vec blob not null,
            sketch64_hex text not null default '',
            bucket12 integer,
            updated_at text not null default '',
            primary key(path, kind, model)
        );
        create index if not exists image_embeddings_kind_model_idx on image_embeddings(kind, model);
        create table if not exists image_exact_group_members (
            asset_id integer primary key,
            representative_asset_id integer not null
        );
        create index if not exists image_exact_group_members_representative_idx
            on image_exact_group_members(representative_asset_id);
        create table if not exists image_exact_group_stats (
            representative_asset_id integer primary key,
            group_key text not null unique,
            member_count integer not null,
            total_bytes integer not null,
            duplicate_bytes integer not null
        );
        create view if not exists image_exact_groups as
            select members.asset_id,
                   stats.group_key,
                   members.representative_asset_id,
                   stats.member_count,
                   stats.total_bytes,
                   stats.duplicate_bytes
            from image_exact_group_members members
            join image_exact_group_stats stats
              on stats.representative_asset_id=members.representative_asset_id;
        create table if not exists image_asset_occurrences (
            asset_id integer not null,
            conversation_table text not null,
            message_rowid integer not null,
            linked_at text not null default '',
            message_time integer,
            message_date text not null default '',
            sender_uin text not null default '',
            conversation_type text not null default '',
            facts_updated_at text not null default '',
            primary key(asset_id, conversation_table, message_rowid)
        );
        create table if not exists image_asset_reference_counts (
            asset_id integer primary key,
            reference_count integer not null
        );
        create index if not exists image_asset_reference_counts_popularity_idx
            on image_asset_reference_counts(reference_count desc, asset_id desc);
        create table if not exists image_exact_reference_counts (
            representative_asset_id integer primary key,
            reference_count integer not null
        );
        create index if not exists image_exact_reference_counts_popularity_idx
            on image_exact_reference_counts(reference_count desc, representative_asset_id desc);
        create table if not exists image_popularity_cache_state (
            id integer primary key check(id=1),
            schema_version integer not null,
            occurrence_row_count integer not null,
            occurrence_generation integer not null default 0,
            refreshed_at text not null
        );
        create table if not exists image_occurrence_content_state (
            id integer primary key check(id=1),
            generation integer not null default 0
        );
        insert or ignore into image_occurrence_content_state(id,generation)
            values(1,0);
        create table if not exists image_index_content_state (
            id integer primary key check(id=1),
            generation integer not null default 0
        );
        insert or ignore into image_index_content_state(id,generation)
            values(1,0);
        create table if not exists image_occurrence_scan_state (
            conversation_table text primary key,
            source_db_path text not null,
            source_db_identity text not null,
            source_db_size integer not null,
            source_db_mtime_unix integer not null,
            last_rowid integer not null default 0,
            max_rowid integer not null default 0,
            rows_scanned integer not null default 0,
            occurrences_linked integer not null default 0,
            completed integer not null default 0,
            linker_version integer not null default 1,
            scan_mode text not null default 'full',
            updated_at text not null default ''
        );
        create table if not exists image_popularity_cube (
            representative_asset_id integer not null,
            message_date text not null,
            sender_uin text not null,
            conversation_table text not null,
            conversation_type text not null,
            reference_count integer not null,
            primary key(
                representative_asset_id, message_date, sender_uin,
                conversation_table, conversation_type
            )
        );
        create index if not exists image_popularity_cube_rank_idx
            on image_popularity_cube(reference_count desc, representative_asset_id);
        create index if not exists image_popularity_cube_sender_cover_idx
            on image_popularity_cube(
                sender_uin, message_date, conversation_table, conversation_type,
                representative_asset_id, reference_count
            );
        create index if not exists image_popularity_cube_conversation_cover_idx
            on image_popularity_cube(
                conversation_table, message_date, sender_uin, conversation_type,
                representative_asset_id, reference_count
            );
        create index if not exists image_popularity_cube_date_cover_idx
            on image_popularity_cube(
                message_date, sender_uin, conversation_table, conversation_type,
                representative_asset_id, reference_count
            );
        create table if not exists image_popularity_daily_conversation (
            message_date text not null,
            conversation_table text not null,
            conversation_type text not null,
            reference_count integer not null,
            primary key(message_date, conversation_table, conversation_type)
        );
        create table if not exists image_popularity_analysis_state (
            conversation_table text primary key,
            last_message_rowid integer not null default 0,
            max_message_rowid integer not null default 0,
            occurrence_count integer not null default 0,
            fact_count integer not null default 0,
            completed integer not null default 0,
            updated_at text not null default ''
        );
        create table if not exists image_occurrence_fact_migration_state (
            id integer primary key check(id=1),
            last_occurrence_rowid integer not null default 0,
            max_occurrence_rowid integer not null default 0,
            completed integer not null default 0,
            updated_at text not null default '',
            scan_mode text not null default 'target_occurrence_rowid_v3',
            processed_occurrences integer not null default 0,
            total_occurrences integer not null default 0
        );
        create table if not exists image_popularity_build_state (
            id integer primary key check(id=1),
            schema_version integer not null,
            occurrence_count integer not null default 0,
            fact_count integer not null default 0,
            cube_row_count integer not null default 0,
            summary_ready integer not null default 0,
            refreshed_at text not null default ''
        );
        create table if not exists image_popularity_summary_cache (
            scope text not null,
            scope_key text not null,
            reference_count integer not null,
            image_count integer not null,
            sender_count integer not null,
            conversation_count integer not null,
            first_date text not null default '',
            last_date text not null default '',
            refreshed_at text not null default '',
            primary key(scope, scope_key)
        );
        create table if not exists image_index_overview_cache (
            id integer primary key check(id=1),
            schema_version integer not null,
            payload_json text not null,
            active_asset_count integer not null,
            grouped_asset_count integer not null,
            embedding_row_count integer not null,
            occurrence_row_count integer not null,
            refreshed_at text not null
        );
        "#,
    )?;
    ensure_column(
        con,
        "image_assets",
        "blur_score",
        "alter table image_assets add column blur_score real",
    )?;
    ensure_column(
        con,
        "image_assets",
        "phash_algo",
        "alter table image_assets add column phash_algo text not null default ''",
    )?;
    ensure_column(
        con,
        "image_assets",
        "blur_algo",
        "alter table image_assets add column blur_algo text not null default ''",
    )?;
    ensure_column(
        con,
        "image_assets",
        "quality_flags",
        "alter table image_assets add column quality_flags text not null default ''",
    )?;
    ensure_column(
        con,
        "image_assets",
        "source_class",
        "alter table image_assets add column source_class text not null default ''",
    )?;
    ensure_column(
        con,
        "image_assets",
        "detected_format",
        "alter table image_assets add column detected_format text not null default ''",
    )?;
    ensure_column(
        con,
        "image_assets",
        "has_alpha",
        "alter table image_assets add column has_alpha integer not null default 0",
    )?;
    ensure_column(
        con,
        "image_assets",
        "orientation_applied",
        "alter table image_assets add column orientation_applied integer not null default 0",
    )?;
    ensure_column(
        con,
        "image_assets",
        "fingerprint_version",
        "alter table image_assets add column fingerprint_version text not null default ''",
    )?;
    con.execute(
        "create index if not exists image_assets_source_class_idx on image_assets(source_class)",
        [],
    )?;
    con.execute(
        "create index if not exists image_assets_phash_algo_idx on image_assets(phash_algo, phash_hex)",
        [],
    )?;
    ensure_column(
        con,
        "image_embeddings",
        "sketch64_hex",
        "alter table image_embeddings add column sketch64_hex text not null default ''",
    )?;
    ensure_column(
        con,
        "image_embeddings",
        "bucket12",
        "alter table image_embeddings add column bucket12 integer",
    )?;
    con.execute(
        "create index if not exists image_embeddings_bucket_idx on image_embeddings(kind, model, dim, bucket12)",
        [],
    )?;
    ensure_column(
        con,
        "image_occurrence_scan_state",
        "linker_version",
        "alter table image_occurrence_scan_state add column linker_version integer not null default 1",
    )?;
    ensure_column(
        con,
        "image_occurrence_scan_state",
        "scan_mode",
        "alter table image_occurrence_scan_state add column scan_mode text not null default 'full'",
    )?;
    ensure_column(
        con,
        "image_asset_occurrences",
        "message_time",
        "alter table image_asset_occurrences add column message_time integer",
    )?;
    ensure_column(
        con,
        "image_asset_occurrences",
        "message_date",
        "alter table image_asset_occurrences add column message_date text not null default ''",
    )?;
    ensure_column(
        con,
        "image_asset_occurrences",
        "sender_uin",
        "alter table image_asset_occurrences add column sender_uin text not null default ''",
    )?;
    ensure_column(
        con,
        "image_asset_occurrences",
        "conversation_type",
        "alter table image_asset_occurrences add column conversation_type text not null default ''",
    )?;
    ensure_column(
        con,
        "image_asset_occurrences",
        "facts_updated_at",
        "alter table image_asset_occurrences add column facts_updated_at text not null default ''",
    )?;
    ensure_column(
        con,
        "image_occurrence_fact_migration_state",
        "scan_mode",
        "alter table image_occurrence_fact_migration_state add column scan_mode text not null default 'occurrence_rowid_v1'",
    )?;
    ensure_column(
        con,
        "image_occurrence_fact_migration_state",
        "processed_occurrences",
        "alter table image_occurrence_fact_migration_state add column processed_occurrences integer not null default 0",
    )?;
    ensure_column(
        con,
        "image_occurrence_fact_migration_state",
        "total_occurrences",
        "alter table image_occurrence_fact_migration_state add column total_occurrences integer not null default 0",
    )?;
    ensure_column(
        con,
        "image_popularity_cache_state",
        "occurrence_generation",
        "alter table image_popularity_cache_state add column occurrence_generation integer not null default 0",
    )?;
    ensure_occurrence_conversation_index(con)?;
    if !exact_groups_are_legacy_table(con)? {
        ensure_exact_group_compatibility_view(con)?;
    }
    Ok(())
}

fn occurrence_content_generation(con: &Connection) -> anyhow::Result<i64> {
    Ok(con.query_row(
        "select generation from image_occurrence_content_state where id=1",
        [],
        |row| row.get(0),
    )?)
}

pub(crate) fn mark_occurrence_content_changed(con: &Connection) -> anyhow::Result<()> {
    con.execute(
        "update image_occurrence_content_state set generation=generation+1 where id=1",
        [],
    )?;
    Ok(())
}

pub(crate) fn mark_image_index_content_changed(con: &Connection) -> anyhow::Result<()> {
    con.execute(
        "update image_index_content_state set generation=generation+1 where id=1",
        [],
    )?;
    Ok(())
}

fn ensure_occurrence_conversation_index(con: &Connection) -> anyhow::Result<()> {
    let exists: bool = con.query_row(
        r#"select exists(
               select 1 from sqlite_master
               where type='index'
                 and name in (
                     'image_occurrences_conversation_idx',
                     'image_asset_occurrences_v3_migration_conversation_idx',
                     'image_occurrence_facts_conversation_idx'
                 )
           )"#,
        [],
        |row| row.get(0),
    )?;
    if !exists {
        con.execute(
            r#"create index image_occurrences_conversation_idx
               on image_asset_occurrences(conversation_table,message_rowid)"#,
            [],
        )?;
    }
    Ok(())
}

fn exact_groups_are_legacy_table(con: &Connection) -> anyhow::Result<bool> {
    Ok(con
        .query_row(
            "select type='table' from sqlite_master where name='image_exact_groups'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// Keeps existing readers compatible while storing group-wide attributes once.
///
/// Large legacy manifests are deliberately converted by `refresh_exact_groups`
/// (the manifest maintenance stage), not by `init_schema`, so an upgrade never
/// delays opening the HTTP listener by minutes.
fn ensure_exact_group_compatibility_view(con: &Connection) -> anyhow::Result<()> {
    con.execute_batch(
        r#"create index if not exists image_exact_group_members_representative_idx
               on image_exact_group_members(representative_asset_id);
           create view if not exists image_exact_groups as
               select members.asset_id,
                      stats.group_key,
                      members.representative_asset_id,
                      stats.member_count,
                      stats.total_bytes,
                      stats.duplicate_bytes
               from image_exact_group_members members
               join image_exact_group_stats stats
                 on stats.representative_asset_id=members.representative_asset_id;
           create trigger if not exists image_exact_groups_insert
               instead of insert on image_exact_groups
               begin
                   insert into image_exact_group_stats(
                       representative_asset_id,group_key,member_count,
                       total_bytes,duplicate_bytes
                   ) values(
                       new.representative_asset_id,new.group_key,new.member_count,
                       new.total_bytes,new.duplicate_bytes
                   )
                   on conflict(representative_asset_id) do update set
                       group_key=excluded.group_key,
                       member_count=excluded.member_count,
                       total_bytes=excluded.total_bytes,
                       duplicate_bytes=excluded.duplicate_bytes;
                   insert into image_exact_group_members(
                       asset_id,representative_asset_id
                   ) values(new.asset_id,new.representative_asset_id)
                   on conflict(asset_id) do update set
                       representative_asset_id=excluded.representative_asset_id;
               end;
           create trigger if not exists image_exact_groups_delete
               instead of delete on image_exact_groups
               begin
                   delete from image_exact_group_members
                   where asset_id=old.asset_id;
                   delete from image_exact_group_stats
                   where representative_asset_id=old.representative_asset_id
                     and not exists(
                         select 1 from image_exact_group_members
                         where representative_asset_id=old.representative_asset_id
                     );
               end;"#,
    )?;
    Ok(())
}

fn configure_manifest_connection(con: &Connection) -> anyhow::Result<()> {
    con.execute_batch(
        r#"
        pragma busy_timeout=5000;
        pragma journal_mode=WAL;
        pragma synchronous=NORMAL;
        pragma temp_store=MEMORY;
        -- Large resumable maintenance jobs write derived rows in small
        -- transactions. The default 1,000-page auto-checkpoint can turn an
        -- otherwise bounded commit (and a pause request) into a minute-long
        -- synchronous copy on Windows-mounted disks. Let the final connection
        -- close checkpoint the WAL instead.
        pragma wal_autocheckpoint=0;
        "#,
    )?;
    Ok(())
}

fn ensure_column(
    con: &Connection,
    table: &str,
    column: &str,
    alter_sql: &str,
) -> anyhow::Result<()> {
    let mut stmt = con.prepare(&format!("pragma table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    con.execute(alter_sql, [])?;
    Ok(())
}

fn attached_column_exists(
    con: &Connection,
    schema: &str,
    table: &str,
    column: &str,
) -> anyhow::Result<bool> {
    let sql = format!("pragma {schema}.table_info({table})");
    let mut stmt = con.prepare(&sql)?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn attached_table_exists(con: &Connection, schema: &str, table: &str) -> anyhow::Result<bool> {
    let sql = format!("select count(*) from {schema}.sqlite_master where type='table' and name=?1");
    let count: i64 = con.query_row(&sql, [table], |row| row.get(0))?;
    Ok(count > 0)
}

fn ensure_attached_image_index_tables(con: &Connection) -> anyhow::Result<()> {
    for table in ["image_assets", "image_embeddings"] {
        if !attached_table_exists(con, "source_manifest", table)? {
            anyhow::bail!("source manifest is missing required table: {table}");
        }
    }
    Ok(())
}

fn scan_roots(options: &BuildOptions) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for path in &options.asset_roots {
        push_existing_dir(&mut roots, path.clone());
    }
    if !roots.is_empty() {
        return roots;
    }
    push_existing_dir(
        &mut roots,
        config::prepared_cfb_root(&options.root, &options.account),
    );
    push_existing_dir(&mut roots, options.root.join(&options.account));
    roots
}

struct RootScanState {
    root: PathBuf,
    walker: ImageFileWalker,
    stats: RootScanReport,
    started: Instant,
}

impl RootScanState {
    fn new(root: PathBuf) -> Self {
        let root_text = root.display().to_string();
        Self {
            walker: ImageFileWalker::new(&root),
            root,
            stats: RootScanReport {
                root: root_text,
                ..RootScanReport::default()
            },
            started: Instant::now(),
        }
    }

    fn finish(mut self) -> RootScanReport {
        self.stats.directories_seen = self.walker.directories_seen;
        self.stats.files_seen = self.walker.files_seen;
        self.stats.walk_errors = self.walker.errors;
        self.stats.elapsed_ms = self.started.elapsed().as_millis() as u64;
        self.stats
    }
}

fn build_manifest_stage(
    con: &mut Connection,
    options: &BuildOptions,
    roots: &[PathBuf],
    report: &mut BuildReport,
) -> anyhow::Result<()> {
    if options.manifest_mode == ManifestMode::Fast {
        return build_manifest_stage_fast(con, options, roots, report);
    }
    if options.manifest_workers > 1 {
        return build_manifest_stage_parallel(con, options, roots, report);
    }
    build_manifest_stage_serial(con, options, roots, report)
}

fn build_manifest_stage_serial(
    con: &mut Connection,
    options: &BuildOptions,
    roots: &[PathBuf],
    report: &mut BuildReport,
) -> anyhow::Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    reset_seen_paths(con)?;
    let mut states = roots
        .iter()
        .cloned()
        .map(RootScanState::new)
        .collect::<Vec<_>>();
    let mut active = (0..states.len()).collect::<VecDeque<_>>();
    let batch_size = manifest_write_batch_size(options);
    let mut pending = Vec::with_capacity(batch_size);
    let mut hit_file_limit = false;
    while let Some(idx) = active.pop_front() {
        if report.scanned_files >= options.max_files {
            active.push_front(idx);
            hit_file_limit = root_queue_has_more_images(&mut states, &mut active);
            break;
        }
        match states[idx].walker.next_with_budget(IMAGE_WALK_ENTRY_BUDGET) {
            ImageWalkStep::Image(path) => {
                process_manifest_path(
                    con,
                    options,
                    report,
                    &mut states[idx],
                    path,
                    &mut pending,
                    batch_size,
                )?;
                active.push_back(idx);
            }
            ImageWalkStep::Pending => active.push_back(idx),
            ImageWalkStep::Done => {}
        }
    }
    let stats = flush_index_batch_unprofiled(con, None, None, &mut pending)?;
    report.indexed_files += stats.indexed;
    report.error_files += stats.errors;
    if !hit_file_limit {
        report.stale_files = mark_missing_assets_stale(con)?;
    }
    report.root_stats = states.into_iter().map(RootScanState::finish).collect();
    Ok(())
}

struct FastManifestItem {
    source_root: String,
    path_text: String,
    file_size: i64,
    mtime_unix: i64,
    source_class: String,
}

fn build_manifest_stage_fast(
    con: &mut Connection,
    options: &BuildOptions,
    roots: &[PathBuf],
    report: &mut BuildReport,
) -> anyhow::Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    reset_seen_paths(con)?;
    let mut seen = HashSet::new();
    let mut states = roots
        .iter()
        .cloned()
        .map(RootScanState::new)
        .collect::<Vec<_>>();
    let mut active = (0..states.len()).collect::<VecDeque<_>>();
    let batch_size = manifest_write_batch_size(options);
    let mut pending_seen = Vec::with_capacity(batch_size);
    let mut pending = Vec::with_capacity(batch_size);
    let mut hit_file_limit = false;
    while let Some(idx) = active.pop_front() {
        if report.scanned_files >= options.max_files {
            active.push_front(idx);
            hit_file_limit = root_queue_has_more_images(&mut states, &mut active);
            break;
        }
        match states[idx].walker.next_with_budget(IMAGE_WALK_ENTRY_BUDGET) {
            ImageWalkStep::Image(path) => {
                process_fast_manifest_path(
                    con,
                    options,
                    report,
                    &mut states[idx],
                    path,
                    &mut seen,
                    &mut pending_seen,
                    &mut pending,
                    batch_size,
                )?;
                active.push_back(idx);
            }
            ImageWalkStep::Pending => active.push_back(idx),
            ImageWalkStep::Done => {}
        }
    }
    let stats = flush_fast_manifest_batch(con, &mut pending_seen, &mut pending)?;
    report.indexed_files += stats.indexed;
    report.error_files += stats.errors;
    if !hit_file_limit {
        report.stale_files = mark_missing_assets_stale(con)?;
    }
    report.root_stats = states.into_iter().map(RootScanState::finish).collect();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_fast_manifest_path(
    con: &mut Connection,
    options: &BuildOptions,
    report: &mut BuildReport,
    state: &mut RootScanState,
    path: PathBuf,
    seen: &mut HashSet<String>,
    pending_seen: &mut Vec<String>,
    pending: &mut Vec<FastManifestItem>,
    batch_size: usize,
) -> anyhow::Result<()> {
    let canonical = path.canonicalize().unwrap_or(path);
    let path_key = canonical.to_string_lossy().to_string();
    if !seen.insert(path_key.clone()) {
        report.skipped_files += 1;
        state.stats.skipped_files += 1;
        return Ok(());
    }
    pending_seen.push(path_key.clone());
    report.scanned_files += 1;
    state.stats.scanned_files += 1;

    let meta = match fs::metadata(&canonical) {
        Ok(meta) => meta,
        Err(err) => {
            report.error_files += 1;
            state.stats.error_files += 1;
            record_path_error(con, &path_key, &err.to_string())?;
            if pending_seen.len() >= batch_size {
                let stats = flush_fast_manifest_batch(con, pending_seen, pending)?;
                report.indexed_files += stats.indexed;
                report.error_files += stats.errors;
            }
            return Ok(());
        }
    };
    let file_size = meta.len() as i64;
    let mtime_unix = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let source_root = state.root.to_string_lossy().to_string();
    let source_class = classify_source_path(&canonical);
    if !options.force
        && existing_fast_unchanged(
            con,
            &path_key,
            &source_root,
            file_size,
            mtime_unix,
            &source_class,
        )?
    {
        report.unchanged_files += 1;
        state.stats.unchanged_files += 1;
    } else {
        pending.push(FastManifestItem {
            source_root,
            path_text: path_key,
            file_size,
            mtime_unix,
            source_class,
        });
        state.stats.indexed_files += 1;
    }
    if pending.len() >= batch_size || pending_seen.len() >= batch_size {
        let stats = flush_fast_manifest_batch(con, pending_seen, pending)?;
        report.indexed_files += stats.indexed;
        report.error_files += stats.errors;
    }
    Ok(())
}

fn flush_fast_manifest_batch(
    con: &mut Connection,
    pending_seen: &mut Vec<String>,
    pending: &mut Vec<FastManifestItem>,
) -> anyhow::Result<FlushStats> {
    if pending_seen.is_empty() && pending.is_empty() {
        return Ok(FlushStats::default());
    }
    let indexed = pending.len();
    let tx = con.transaction()?;
    {
        let mut seen_stmt =
            tx.prepare_cached("insert or ignore into image_index_seen_paths(path) values (?1)")?;
        for path in pending_seen.drain(..) {
            seen_stmt.execute([path])?;
        }
    }
    let indexed_at = current_timestamp();
    {
        let mut asset_stmt = tx.prepare_cached(
            r#"
            insert into image_assets
                (path, source_root, file_size, mtime_unix, sha256_hex, phash_hex, phash_algo, width, height, blur_score, blur_algo, quality_flags, source_class, detected_format, has_alpha, orientation_applied, fingerprint_version, indexed_at, stale, error)
            values
                (?1, ?2, ?3, ?4, '', null, '', null, null, null, '', ?5, ?6, '', 0, 0, '', ?7, 0, null)
            on conflict(path) do update set
                source_root=excluded.source_root,
                file_size=excluded.file_size,
                mtime_unix=excluded.mtime_unix,
                sha256_hex='',
                phash_hex=null,
                phash_algo='',
                width=null,
                height=null,
                blur_score=null,
                blur_algo='',
                quality_flags=excluded.quality_flags,
                source_class=excluded.source_class,
                detected_format='',
                has_alpha=0,
                orientation_applied=0,
                fingerprint_version='',
                indexed_at=excluded.indexed_at,
                stale=0,
                error=null
            "#,
        )?;
        for item in pending.drain(..) {
            asset_stmt.execute(params![
                item.path_text,
                item.source_root,
                item.file_size,
                item.mtime_unix,
                FAST_MANIFEST_PENDING_FLAG,
                item.source_class,
                indexed_at,
            ])?;
            delete_derived_features_for_path(&tx, &item.path_text)?;
        }
    }
    tx.commit()?;
    Ok(FlushStats { indexed, errors: 0 })
}

struct ManifestJob {
    root: PathBuf,
    root_idx: usize,
    path: PathBuf,
    path_key: String,
    file_size: i64,
    mtime_unix: i64,
}

enum ManifestJobResult {
    Indexed {
        root_idx: usize,
        item: Box<PendingIndexImage>,
    },
    Error {
        root_idx: usize,
        path_key: String,
        error: String,
    },
}

fn build_manifest_stage_parallel(
    con: &mut Connection,
    options: &BuildOptions,
    roots: &[PathBuf],
    report: &mut BuildReport,
) -> anyhow::Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    reset_seen_paths(con)?;
    let worker_count = options.manifest_workers.clamp(1, MAX_MANIFEST_WORKERS);
    let mut states = roots
        .iter()
        .cloned()
        .map(RootScanState::new)
        .collect::<Vec<_>>();
    let mut active = (0..states.len()).collect::<VecDeque<_>>();
    let (result_tx, result_rx) = mpsc::channel::<ManifestJobResult>();
    let mut job_senders = Vec::with_capacity(worker_count);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let (job_tx, job_rx) = mpsc::channel::<ManifestJob>();
        let result_tx = result_tx.clone();
        workers.push(std::thread::spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let result = prepare_index_image_from_metadata(
                    &job.root,
                    &job.path,
                    fs_path_for_host(&job.path).as_ref(),
                    &job.path_key,
                    job.file_size,
                    job.mtime_unix,
                    false,
                    None,
                )
                .map(|item| ManifestJobResult::Indexed {
                    root_idx: job.root_idx,
                    item: Box::new(item),
                })
                .unwrap_or_else(|err| ManifestJobResult::Error {
                    root_idx: job.root_idx,
                    path_key: job.path_key,
                    error: err.to_string(),
                });
                let _ = result_tx.send(result);
            }
        }));
        job_senders.push(job_tx);
    }
    drop(result_tx);

    let batch_size = manifest_write_batch_size(options);
    let mut pending = Vec::with_capacity(batch_size);
    let mut sent_jobs = 0usize;
    let mut received_jobs = 0usize;
    let mut next_worker = 0usize;
    let mut hit_file_limit = false;
    while let Some(idx) = active.pop_front() {
        drain_manifest_results(
            con,
            report,
            &mut states,
            &mut pending,
            &result_rx,
            &mut received_jobs,
            sent_jobs,
            batch_size,
            false,
        )?;
        if report.scanned_files >= options.max_files {
            active.push_front(idx);
            hit_file_limit = root_queue_has_more_images(&mut states, &mut active);
            break;
        }
        match states[idx].walker.next_with_budget(IMAGE_WALK_ENTRY_BUDGET) {
            ImageWalkStep::Image(path) => {
                if let Some(job) =
                    prepare_manifest_job(con, options, report, &mut states[idx], idx, path)?
                {
                    let sender_idx = next_worker % job_senders.len();
                    next_worker += 1;
                    if job_senders[sender_idx].send(job).is_ok() {
                        sent_jobs += 1;
                    } else {
                        anyhow::bail!("manifest worker channel closed unexpectedly");
                    }
                }
                active.push_back(idx);
            }
            ImageWalkStep::Pending => active.push_back(idx),
            ImageWalkStep::Done => {}
        }
    }
    drop(job_senders);
    drain_manifest_results(
        con,
        report,
        &mut states,
        &mut pending,
        &result_rx,
        &mut received_jobs,
        sent_jobs,
        batch_size,
        true,
    )?;
    for worker in workers {
        let _ = worker.join();
    }
    if received_jobs != sent_jobs {
        anyhow::bail!("manifest workers returned {received_jobs} results for {sent_jobs} jobs");
    }
    let stats = flush_index_batch_unprofiled(con, None, None, &mut pending)?;
    report.indexed_files += stats.indexed;
    report.error_files += stats.errors;
    if !hit_file_limit {
        report.stale_files = mark_missing_assets_stale(con)?;
    }
    report.root_stats = states.into_iter().map(RootScanState::finish).collect();
    Ok(())
}

fn prepare_manifest_job(
    con: &Connection,
    options: &BuildOptions,
    report: &mut BuildReport,
    state: &mut RootScanState,
    root_idx: usize,
    path: PathBuf,
) -> anyhow::Result<Option<ManifestJob>> {
    let canonical = path.canonicalize().unwrap_or(path);
    let path_key = canonical.to_string_lossy().to_string();
    if !remember_seen_path(con, &path_key)? {
        report.skipped_files += 1;
        state.stats.skipped_files += 1;
        return Ok(None);
    }
    report.scanned_files += 1;
    state.stats.scanned_files += 1;
    let meta = match fs::metadata(&canonical) {
        Ok(meta) => meta,
        Err(err) => {
            report.error_files += 1;
            state.stats.error_files += 1;
            record_path_error(con, &path_key, &err.to_string())?;
            return Ok(None);
        }
    };
    let file_size = meta.len() as i64;
    let mtime_unix = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    if !options.force && existing_unchanged(con, &path_key, file_size, mtime_unix, &[])? {
        report.unchanged_files += 1;
        state.stats.unchanged_files += 1;
        return Ok(None);
    }
    Ok(Some(ManifestJob {
        root: state.root.clone(),
        root_idx,
        path: canonical,
        path_key,
        file_size,
        mtime_unix,
    }))
}

#[allow(clippy::too_many_arguments)]
fn drain_manifest_results(
    con: &mut Connection,
    report: &mut BuildReport,
    states: &mut [RootScanState],
    pending: &mut Vec<PendingIndexImage>,
    result_rx: &mpsc::Receiver<ManifestJobResult>,
    received_jobs: &mut usize,
    sent_jobs: usize,
    batch_size: usize,
    wait_all: bool,
) -> anyhow::Result<()> {
    loop {
        let result = if wait_all && *received_jobs < sent_jobs {
            result_rx.recv().ok()
        } else {
            result_rx.try_recv().ok()
        };
        let Some(result) = result else {
            break;
        };
        *received_jobs += 1;
        match result {
            ManifestJobResult::Indexed { root_idx, item } => {
                if let Some(state) = states.get_mut(root_idx) {
                    state.stats.indexed_files += 1;
                }
                pending.push(*item);
                if pending.len() >= batch_size {
                    let stats = flush_index_batch_unprofiled(con, None, None, pending)?;
                    report.indexed_files += stats.indexed;
                    report.error_files += stats.errors;
                }
            }
            ManifestJobResult::Error {
                root_idx,
                path_key,
                error,
            } => {
                report.error_files += 1;
                if let Some(state) = states.get_mut(root_idx) {
                    state.stats.error_files += 1;
                }
                record_path_error(con, &path_key, &error)?;
            }
        }
    }
    Ok(())
}

fn process_manifest_path(
    con: &mut Connection,
    options: &BuildOptions,
    report: &mut BuildReport,
    state: &mut RootScanState,
    path: PathBuf,
    pending: &mut Vec<PendingIndexImage>,
    batch_size: usize,
) -> anyhow::Result<()> {
    let canonical = path.canonicalize().unwrap_or(path);
    let path_key = canonical.to_string_lossy().to_string();
    if !remember_seen_path(con, &path_key)? {
        report.skipped_files += 1;
        state.stats.skipped_files += 1;
        return Ok(());
    }
    report.scanned_files += 1;
    state.stats.scanned_files += 1;
    match prepare_index_image(
        con,
        &state.root,
        &canonical,
        options.force,
        &[],
        false,
        None,
    ) {
        Ok(Some(item)) => {
            pending.push(item);
            state.stats.indexed_files += 1;
            if pending.len() >= batch_size {
                let stats = flush_index_batch_unprofiled(con, None, None, pending)?;
                report.indexed_files += stats.indexed;
                report.error_files += stats.errors;
            }
        }
        Ok(None) => {
            report.unchanged_files += 1;
            state.stats.unchanged_files += 1;
        }
        Err(err) => {
            report.error_files += 1;
            state.stats.error_files += 1;
            record_path_error(con, &path_key, &err.to_string())?;
        }
    }
    Ok(())
}

struct BenchmarkSqliteWriter {
    con: Connection,
    pending: Vec<PendingIndexImage>,
    batch_size: usize,
}

impl BenchmarkSqliteWriter {
    fn open(
        path: PathBuf,
        batch_size: usize,
        report: &mut ManifestBenchmarkReport,
    ) -> anyhow::Result<Self> {
        let started = Instant::now();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let con = Connection::open(path)?;
        init_schema(&con)?;
        report.timing.elapsed_sqlite_ms += elapsed_ms_f64(started);
        Ok(Self {
            con,
            pending: Vec::with_capacity(batch_size.max(1)),
            batch_size: batch_size.max(1),
        })
    }

    fn push(
        &mut self,
        item: PendingIndexImage,
        report: &mut ManifestBenchmarkReport,
        root: &mut ManifestBenchmarkRootReport,
    ) -> anyhow::Result<()> {
        self.pending.push(item);
        if self.pending.len() >= self.batch_size {
            self.flush(report, root)?;
        }
        Ok(())
    }

    fn flush(
        &mut self,
        report: &mut ManifestBenchmarkReport,
        root: &mut ManifestBenchmarkRootReport,
    ) -> anyhow::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(&mut self.pending);
        let started = Instant::now();
        let tx = self.con.transaction()?;
        let indexed_at = current_timestamp();
        for item in &items {
            write_asset_row(&tx, item, &indexed_at)?;
        }
        tx.commit()?;
        let elapsed = elapsed_ms_f64(started);
        report.timing.elapsed_sqlite_ms += elapsed;
        root.timing.elapsed_sqlite_ms += elapsed;
        report.sqlite_insert_count += items.len();
        root.sqlite_insert_count += items.len();
        Ok(())
    }

    fn flush_without_root(&mut self, report: &mut ManifestBenchmarkReport) -> anyhow::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(&mut self.pending);
        let started = Instant::now();
        let tx = self.con.transaction()?;
        let indexed_at = current_timestamp();
        for item in &items {
            write_asset_row(&tx, item, &indexed_at)?;
        }
        tx.commit()?;
        report.timing.elapsed_sqlite_ms += elapsed_ms_f64(started);
        report.sqlite_insert_count += items.len();
        Ok(())
    }
}

fn benchmark_enum_backend() -> &'static str {
    if cfg!(windows) {
        "win32_find_first_file_ex"
    } else {
        "std_read_dir"
    }
}

fn benchmark_walk_root(
    root: &Path,
    options: &ManifestBenchmarkOptions,
    report: &mut ManifestBenchmarkReport,
    root_report: &mut ManifestBenchmarkRootReport,
    sqlite: Option<&mut BenchmarkSqliteWriter>,
    decode_profile: Option<&mut BenchmarkDecodeProfile>,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        return benchmark_walk_root_win32(
            root,
            options,
            report,
            root_report,
            sqlite,
            decode_profile,
        );
    }
    #[cfg(not(windows))]
    {
        benchmark_walk_root_std(root, options, report, root_report, sqlite, decode_profile)
    }
}

#[cfg(not(windows))]
fn benchmark_walk_root_std(
    root: &Path,
    options: &ManifestBenchmarkOptions,
    report: &mut ManifestBenchmarkReport,
    root_report: &mut ManifestBenchmarkRootReport,
    mut sqlite: Option<&mut BenchmarkSqliteWriter>,
    mut decode_profile: Option<&mut BenchmarkDecodeProfile>,
) -> anyhow::Result<()> {
    let mut pending_dirs = vec![root.to_path_buf()];
    while let Some(dir) = pending_dirs.pop() {
        if report.processed_files >= options.max_files {
            break;
        }
        report.directories_seen += 1;
        root_report.directories_seen += 1;
        report.find_first_count += 1;
        root_report.find_first_count += 1;
        let started = Instant::now();
        let entries = fs::read_dir(&dir);
        add_find_elapsed(report, root_report, started);
        let mut entries = match entries {
            Ok(entries) => entries,
            Err(_) => {
                report.walk_errors += 1;
                root_report.walk_errors += 1;
                continue;
            }
        };
        loop {
            if report.processed_files >= options.max_files {
                break;
            }
            report.find_next_count += 1;
            root_report.find_next_count += 1;
            let started = Instant::now();
            let next = entries.next();
            add_find_elapsed(report, root_report, started);
            let Some(entry) = next else {
                break;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    report.walk_errors += 1;
                    root_report.walk_errors += 1;
                    continue;
                }
            };
            let started = Instant::now();
            let ty = entry.file_type();
            add_find_elapsed(report, root_report, started);
            let path = entry.path();
            match ty {
                Ok(ty) if ty.is_dir() => pending_dirs.push(path),
                Ok(ty) if ty.is_file() => {
                    report.files_seen += 1;
                    root_report.files_seen += 1;
                    if is_supported_image_path(&path) {
                        benchmark_process_supported_path(
                            root,
                            &path,
                            options,
                            report,
                            root_report,
                            sqlite.as_deref_mut(),
                            decode_profile.as_deref_mut(),
                        )?;
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    report.walk_errors += 1;
                    root_report.walk_errors += 1;
                }
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn benchmark_walk_root_win32(
    root: &Path,
    options: &ManifestBenchmarkOptions,
    report: &mut ManifestBenchmarkReport,
    root_report: &mut ManifestBenchmarkRootReport,
    mut sqlite: Option<&mut BenchmarkSqliteWriter>,
    mut decode_profile: Option<&mut BenchmarkDecodeProfile>,
) -> anyhow::Result<()> {
    let mut pending_dirs = vec![root.to_path_buf()];
    while let Some(dir) = pending_dirs.pop() {
        if report.processed_files >= options.max_files {
            break;
        }
        report.directories_seen += 1;
        root_report.directories_seen += 1;
        report.find_first_count += 1;
        root_report.find_first_count += 1;
        let pattern = dir.join("*");
        let pattern_wide = windows_path_wide(&pattern);
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let started = Instant::now();
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
        add_find_elapsed(report, root_report, started);
        if handle == INVALID_HANDLE_VALUE {
            report.walk_errors += 1;
            root_report.walk_errors += 1;
            continue;
        }
        loop {
            if report.processed_files >= options.max_files {
                break;
            }
            if let Some(name) = win32_find_file_name(&data) {
                let path = dir.join(name);
                if data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    pending_dirs.push(path);
                } else {
                    report.files_seen += 1;
                    root_report.files_seen += 1;
                    if is_supported_image_path(&path) {
                        benchmark_process_supported_path(
                            root,
                            &path,
                            options,
                            report,
                            root_report,
                            sqlite.as_deref_mut(),
                            decode_profile.as_deref_mut(),
                        )?;
                    }
                }
            }
            report.find_next_count += 1;
            root_report.find_next_count += 1;
            let started = Instant::now();
            let ok = unsafe { FindNextFileW(handle, &mut data) };
            add_find_elapsed(report, root_report, started);
            if ok == 0 {
                break;
            }
        }
        unsafe {
            FindClose(handle);
        }
    }
    Ok(())
}

fn benchmark_process_supported_path(
    source_root: &Path,
    path: &Path,
    options: &ManifestBenchmarkOptions,
    report: &mut ManifestBenchmarkReport,
    root_report: &mut ManifestBenchmarkRootReport,
    sqlite: Option<&mut BenchmarkSqliteWriter>,
    decode_profile: Option<&mut BenchmarkDecodeProfile>,
) -> anyhow::Result<()> {
    if report.processed_files >= options.max_files {
        return Ok(());
    }
    report.supported_files += 1;
    root_report.supported_files += 1;
    report.processed_files += 1;
    root_report.processed_files += 1;
    match options.mode {
        ManifestBenchmarkMode::EnumOnly => {}
        ManifestBenchmarkMode::EnumPlusOpen0 => {
            benchmark_open_zero(path, report, root_report);
        }
        ManifestBenchmarkMode::EnumPlusFileInfo => {
            benchmark_fileinfo(path, report, root_report);
        }
        ManifestBenchmarkMode::EnumPlusHeader => {
            benchmark_read_header(path, options.header_bytes, report, root_report);
        }
        ManifestBenchmarkMode::EnumPlusDecode => {
            if let Some(bytes) = benchmark_read_full(path, report, root_report) {
                benchmark_decode(&bytes, report, root_report);
            }
        }
        ManifestBenchmarkMode::FullManifestNoSqlite => {
            benchmark_full_manifest_no_sqlite(path, report, root_report);
        }
        ManifestBenchmarkMode::FingerprintNoSqlite => {
            benchmark_fingerprint_no_sqlite(path, report, root_report);
        }
        ManifestBenchmarkMode::TurboJpegFingerprintNoSqlite => {
            benchmark_turbojpeg_fingerprint_no_sqlite(path, report, root_report);
        }
        ManifestBenchmarkMode::SqliteBatch => {
            benchmark_sqlite_batch_item(source_root, path, report, root_report, sqlite)?;
        }
        ManifestBenchmarkMode::DecodeProfile => {
            if let Some(profile) = decode_profile {
                benchmark_decode_profile_path(
                    path,
                    report,
                    root_report,
                    profile,
                    DecodeProfileBackend::Image,
                );
            }
        }
        ManifestBenchmarkMode::JpegLumaDecodeProfile => {
            if let Some(profile) = decode_profile {
                benchmark_decode_profile_path(
                    path,
                    report,
                    root_report,
                    profile,
                    DecodeProfileBackend::ZuneJpegLuma,
                );
            }
        }
        ManifestBenchmarkMode::TurboJpegDecodeProfile => {
            if let Some(profile) = decode_profile {
                benchmark_decode_profile_path(
                    path,
                    report,
                    root_report,
                    profile,
                    DecodeProfileBackend::TurboJpegLuma,
                );
            }
        }
    }
    Ok(())
}

fn benchmark_open_zero(
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) {
    report.create_file_count += 1;
    root.create_file_count += 1;
    let started = Instant::now();
    let ok = platform_open_zero(path);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_createfile_ms += elapsed;
    root.timing.elapsed_createfile_ms += elapsed;
    if ok.is_err() {
        report.open_errors += 1;
        root.open_errors += 1;
        report.error_files += 1;
        root.error_files += 1;
    }
}

fn benchmark_fileinfo(
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) {
    #[cfg(windows)]
    {
        report.create_file_count += 1;
        root.create_file_count += 1;
        let started = Instant::now();
        let result = win32_open_zero_handle(path);
        let elapsed = elapsed_ms_f64(started);
        report.timing.elapsed_createfile_ms += elapsed;
        root.timing.elapsed_createfile_ms += elapsed;
        let handle = match result {
            Ok(handle) => handle,
            Err(_) => {
                report.open_errors += 1;
                root.open_errors += 1;
                report.error_files += 1;
                root.error_files += 1;
                return;
            }
        };
        report.fileinfo_count += 1;
        root.fileinfo_count += 1;
        let started = Instant::now();
        let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                windows_sys::Win32::Storage::FileSystem::FileBasicInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        let elapsed = elapsed_ms_f64(started);
        report.timing.elapsed_fileinfo_ms += elapsed;
        root.timing.elapsed_fileinfo_ms += elapsed;
        unsafe {
            CloseHandle(handle);
        }
        if ok == 0 {
            report.fileinfo_errors += 1;
            root.fileinfo_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
        }
    }
    #[cfg(not(windows))]
    {
        report.fileinfo_count += 1;
        root.fileinfo_count += 1;
        let started = Instant::now();
        let ok = fs_metadata_for_host(path).is_ok();
        let elapsed = elapsed_ms_f64(started);
        report.timing.elapsed_fileinfo_ms += elapsed;
        root.timing.elapsed_fileinfo_ms += elapsed;
        if !ok {
            report.fileinfo_errors += 1;
            root.fileinfo_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
        }
    }
}

fn benchmark_read_header(
    path: &Path,
    header_bytes: usize,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) {
    report.create_file_count += 1;
    root.create_file_count += 1;
    let started = Instant::now();
    let file = fs_file_open_for_host(path);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_createfile_ms += elapsed;
    root.timing.elapsed_createfile_ms += elapsed;
    let mut file = match file {
        Ok(file) => file,
        Err(_) => {
            report.open_errors += 1;
            root.open_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
            return;
        }
    };
    report.read_count += 1;
    root.read_count += 1;
    let mut buf = vec![0u8; header_bytes.max(1)];
    let started = Instant::now();
    let result = file.read(&mut buf);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_read_ms += elapsed;
    root.timing.elapsed_read_ms += elapsed;
    match result {
        Ok(bytes) => {
            report.bytes_read += bytes as u64;
            root.bytes_read += bytes as u64;
        }
        Err(_) => {
            report.read_errors += 1;
            root.read_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
        }
    }
}

fn benchmark_read_full(
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) -> Option<Vec<u8>> {
    report.create_file_count += 1;
    root.create_file_count += 1;
    let started = Instant::now();
    let file = fs_file_open_for_host(path);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_createfile_ms += elapsed;
    root.timing.elapsed_createfile_ms += elapsed;
    let mut file = match file {
        Ok(file) => file,
        Err(_) => {
            report.open_errors += 1;
            root.open_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
            return None;
        }
    };
    report.read_count += 1;
    root.read_count += 1;
    let mut bytes = Vec::new();
    let started = Instant::now();
    let result = file.read_to_end(&mut bytes);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_read_ms += elapsed;
    root.timing.elapsed_read_ms += elapsed;
    match result {
        Ok(bytes_read) => {
            report.bytes_read += bytes_read as u64;
            root.bytes_read += bytes_read as u64;
            Some(bytes)
        }
        Err(_) => {
            report.read_errors += 1;
            root.read_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
            None
        }
    }
}

fn benchmark_decode(
    bytes: &[u8],
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) -> Option<DynamicImage> {
    report.decode_count += 1;
    root.decode_count += 1;
    let started = Instant::now();
    let result = image::load_from_memory(bytes);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_decode_ms += elapsed;
    root.timing.elapsed_decode_ms += elapsed;
    match result {
        Ok(image) => Some(image),
        Err(_) => {
            report.decode_errors += 1;
            root.decode_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
            None
        }
    }
}

#[derive(Default)]
struct BenchmarkDecodeProfile {
    by_format: HashMap<String, DecodeProfileBucket>,
    by_format_megapixels: HashMap<String, DecodeProfileBucket>,
    by_format_file_size: HashMap<String, DecodeProfileBucket>,
    by_format_animation: HashMap<String, DecodeProfileBucket>,
}

impl BenchmarkDecodeProfile {
    fn record(&mut self, sample: DecodeProfileSample) {
        let format_key = sample.format.clone();
        self.by_format
            .entry(format_key.clone())
            .or_insert_with(|| DecodeProfileBucket::new("format", &sample.format, None, None, None))
            .push(&sample);

        let mp_key = format!("{}|{}", sample.format, sample.megapixels_bucket);
        self.by_format_megapixels
            .entry(mp_key)
            .or_insert_with(|| {
                DecodeProfileBucket::new(
                    "format_megapixels",
                    &sample.format,
                    Some(sample.megapixels_bucket.clone()),
                    None,
                    None,
                )
            })
            .push(&sample);

        let size_key = format!("{}|{}", sample.format, sample.file_size_bucket);
        self.by_format_file_size
            .entry(size_key)
            .or_insert_with(|| {
                DecodeProfileBucket::new(
                    "format_file_size",
                    &sample.format,
                    None,
                    Some(sample.file_size_bucket.clone()),
                    None,
                )
            })
            .push(&sample);

        let animation_key = format!("{}|{}", sample.format, sample.animation);
        self.by_format_animation
            .entry(animation_key)
            .or_insert_with(|| {
                DecodeProfileBucket::new(
                    "format_animation",
                    &sample.format,
                    None,
                    None,
                    Some(sample.animation.clone()),
                )
            })
            .push(&sample);
    }

    fn finish(self) -> DecodeProfileReport {
        DecodeProfileReport {
            by_format: finalize_decode_profile_buckets(self.by_format),
            by_format_megapixels: finalize_decode_profile_buckets(self.by_format_megapixels),
            by_format_file_size: finalize_decode_profile_buckets(self.by_format_file_size),
            by_format_animation: finalize_decode_profile_buckets(self.by_format_animation),
        }
    }
}

struct DecodeProfileSample {
    format: String,
    file_size_bucket: String,
    megapixels_bucket: String,
    animation: String,
    bytes: u64,
    pixels: u64,
    decode_ms: f64,
    animated: bool,
    error: bool,
}

#[derive(Clone, Copy)]
enum DecodeProfileBackend {
    Image,
    ZuneJpegLuma,
    TurboJpegLuma,
}

struct DecodeProfileBucket {
    group: String,
    format: String,
    megapixels_bucket: Option<String>,
    file_size_bucket: Option<String>,
    animation: Option<String>,
    count: usize,
    total_bytes: u64,
    total_pixels: u64,
    decode_ms_total: f64,
    animated_count: usize,
    error_count: usize,
    decode_samples_ms: Vec<f64>,
    bytes_samples: Vec<u64>,
    pixels_samples: Vec<u64>,
}

impl DecodeProfileBucket {
    fn new(
        group: &str,
        format: &str,
        megapixels_bucket: Option<String>,
        file_size_bucket: Option<String>,
        animation: Option<String>,
    ) -> Self {
        Self {
            group: group.to_string(),
            format: format.to_string(),
            megapixels_bucket,
            file_size_bucket,
            animation,
            count: 0,
            total_bytes: 0,
            total_pixels: 0,
            decode_ms_total: 0.0,
            animated_count: 0,
            error_count: 0,
            decode_samples_ms: Vec::new(),
            bytes_samples: Vec::new(),
            pixels_samples: Vec::new(),
        }
    }

    fn push(&mut self, sample: &DecodeProfileSample) {
        self.count += 1;
        self.total_bytes = self.total_bytes.saturating_add(sample.bytes);
        self.total_pixels = self.total_pixels.saturating_add(sample.pixels);
        self.decode_ms_total += sample.decode_ms;
        if sample.animated {
            self.animated_count += 1;
        }
        if sample.error {
            self.error_count += 1;
        }
        self.decode_samples_ms.push(sample.decode_ms);
        self.bytes_samples.push(sample.bytes);
        if sample.pixels > 0 {
            self.pixels_samples.push(sample.pixels);
        }
    }

    fn finish(mut self) -> DecodeProfileBucketReport {
        self.decode_samples_ms
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        self.bytes_samples.sort_unstable();
        self.pixels_samples.sort_unstable();
        DecodeProfileBucketReport {
            group: self.group,
            format: self.format,
            megapixels_bucket: self.megapixels_bucket,
            file_size_bucket: self.file_size_bucket,
            animation: self.animation,
            count: self.count,
            total_bytes: self.total_bytes,
            total_pixels: self.total_pixels,
            decode_ms_total: round_ms(self.decode_ms_total),
            decode_ms_avg: round_ms(if self.count == 0 {
                0.0
            } else {
                self.decode_ms_total / self.count as f64
            }),
            decode_ms_p50: round_ms(percentile_f64(&self.decode_samples_ms, 0.50)),
            decode_ms_p95: round_ms(percentile_f64(&self.decode_samples_ms, 0.95)),
            decode_ms_p99: round_ms(percentile_f64(&self.decode_samples_ms, 0.99)),
            bytes_p95: percentile_u64(&self.bytes_samples, 0.95),
            pixels_p95: percentile_u64(&self.pixels_samples, 0.95),
            animated_count: self.animated_count,
            error_count: self.error_count,
        }
    }
}

fn finalize_decode_profile_buckets(
    buckets: HashMap<String, DecodeProfileBucket>,
) -> Vec<DecodeProfileBucketReport> {
    let mut rows = buckets
        .into_values()
        .map(DecodeProfileBucket::finish)
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.decode_ms_total
            .partial_cmp(&a.decode_ms_total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.format.cmp(&b.format))
            .then_with(|| a.megapixels_bucket.cmp(&b.megapixels_bucket))
            .then_with(|| a.file_size_bucket.cmp(&b.file_size_bucket))
            .then_with(|| a.animation.cmp(&b.animation))
    });
    rows
}

fn benchmark_decode_profile_path(
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
    profile: &mut BenchmarkDecodeProfile,
    backend: DecodeProfileBackend,
) {
    let Some((canonical, file_size, _)) = benchmark_manifest_path_metadata(path, report, root)
    else {
        return;
    };
    let Some(bytes) = benchmark_read_full(&canonical, report, root) else {
        return;
    };
    let format = image_format_label(&bytes, &canonical);
    let animation = animation_state_label(&format, &bytes);
    let animated = animation == "animated";
    report.decode_count += 1;
    root.decode_count += 1;
    let started = Instant::now();
    let result = benchmark_decode_profile_bytes(&canonical, &bytes, backend);
    let decode_ms = elapsed_ms_f64(started);
    report.timing.elapsed_decode_ms += decode_ms;
    root.timing.elapsed_decode_ms += decode_ms;
    let (pixels, error) = match result {
        Ok((width, height)) => ((width as u64).saturating_mul(height as u64), false),
        Err(_) => {
            report.decode_errors += 1;
            root.decode_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
            (0, true)
        }
    };
    profile.record(DecodeProfileSample {
        format,
        file_size_bucket: file_size_bucket(file_size as u64),
        megapixels_bucket: megapixels_bucket(pixels),
        animation,
        bytes: bytes.len() as u64,
        pixels,
        decode_ms,
        animated,
        error,
    });
}

fn benchmark_decode_profile_bytes(
    _path: &Path,
    bytes: &[u8],
    backend: DecodeProfileBackend,
) -> anyhow::Result<(u32, u32)> {
    match backend {
        DecodeProfileBackend::Image => {
            let image = image::load_from_memory(bytes)?;
            Ok(image.dimensions())
        }
        DecodeProfileBackend::ZuneJpegLuma
            if is_jpeg_bytes(bytes) && !jpeg_orientation_requires_dynamic_path(bytes) =>
        {
            let (width, height, _) = zune_jpeg_luma_decode(bytes)?;
            Ok((width, height))
        }
        DecodeProfileBackend::TurboJpegLuma
            if is_jpeg_bytes(bytes) && !jpeg_orientation_requires_dynamic_path(bytes) =>
        {
            #[cfg(windows)]
            {
                let (width, height, _) = turbojpeg_luma_decode(bytes)?;
                Ok((width, height))
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("turbojpeg_decode_profile requires Windows")
            }
        }
        DecodeProfileBackend::ZuneJpegLuma | DecodeProfileBackend::TurboJpegLuma => {
            let image = image::load_from_memory(bytes)?;
            Ok(image.dimensions())
        }
    }
}

fn percentile_f64(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = percentile_index(sorted.len(), percentile);
    sorted[idx]
}

fn percentile_u64(sorted: &[u64], percentile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = percentile_index(sorted.len(), percentile);
    sorted[idx]
}

fn percentile_index(len: usize, percentile: f64) -> usize {
    if len <= 1 {
        return 0;
    }
    let rank = ((len as f64 - 1.0) * percentile).ceil() as usize;
    rank.min(len - 1)
}

fn round_ms(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn image_format_label(bytes: &[u8], path: &Path) -> String {
    if is_jpeg_bytes(bytes) {
        return "JPEG".to_string();
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "PNG".to_string();
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "GIF".to_string();
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return "WebP".to_string();
    }
    if bytes.starts_with(b"BM") {
        return "BMP".to_string();
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return "TIFF".to_string();
    }
    if is_heif_like(bytes) {
        return "HEIC".to_string();
    }
    match image::guess_format(bytes) {
        Ok(format) => image_format_name(format).to_string(),
        Err(_) => extension_format_label(path).to_string(),
    }
}

fn is_jpeg_bytes(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xff, 0xd8, 0xff])
}

fn image_format_name(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Jpeg => "JPEG",
        image::ImageFormat::Png => "PNG",
        image::ImageFormat::Gif => "GIF",
        image::ImageFormat::WebP => "WebP",
        image::ImageFormat::Bmp => "BMP",
        image::ImageFormat::Tiff => "TIFF",
        image::ImageFormat::Ico => "ICO",
        image::ImageFormat::Pnm => "PNM",
        image::ImageFormat::Tga => "TGA",
        image::ImageFormat::Dds => "DDS",
        image::ImageFormat::Hdr => "HDR",
        image::ImageFormat::OpenExr => "OpenEXR",
        image::ImageFormat::Farbfeld => "Farbfeld",
        image::ImageFormat::Avif => "AVIF",
        image::ImageFormat::Qoi => "QOI",
        _ => "Other",
    }
}

fn extension_format_label(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" | "jpe" => "JPEG",
        "png" => "PNG",
        "webp" => "WebP",
        "gif" => "GIF",
        "bmp" | "dib" => "BMP",
        "tif" | "tiff" => "TIFF",
        "heic" | "heif" | "avci" | "avif" => "HEIC",
        _ => "Unknown",
    }
}

fn is_heif_like(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let brands = &bytes[8..bytes.len().min(64)];
    for brand in [
        b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"mif1", b"msf1",
    ] {
        if brands.windows(4).any(|window| window == brand) {
            return true;
        }
    }
    false
}

fn file_size_bucket(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes < 256 * KB {
        "<256KB".to_string()
    } else if bytes < MB {
        "256KB-1MB".to_string()
    } else if bytes < 4 * MB {
        "1-4MB".to_string()
    } else if bytes < 16 * MB {
        "4-16MB".to_string()
    } else {
        ">16MB".to_string()
    }
}

fn megapixels_bucket(pixels: u64) -> String {
    if pixels == 0 {
        "unknown".to_string()
    } else if pixels < 500_000 {
        "<0.5MP".to_string()
    } else if pixels < 2_000_000 {
        "0.5-2MP".to_string()
    } else if pixels < 8_000_000 {
        "2-8MP".to_string()
    } else if pixels < 20_000_000 {
        "8-20MP".to_string()
    } else {
        ">20MP".to_string()
    }
}

fn animation_state_label(format: &str, bytes: &[u8]) -> String {
    match format {
        "GIF" => {
            if gif_frame_count(bytes) > 1 {
                "animated".to_string()
            } else {
                "static".to_string()
            }
        }
        "WebP" => {
            if webp_has_animation(bytes) {
                "animated".to_string()
            } else {
                "static".to_string()
            }
        }
        "PNG" => {
            if png_has_animation(bytes) {
                "animated".to_string()
            } else {
                "static".to_string()
            }
        }
        "Unknown" => "unknown".to_string(),
        _ => "static".to_string(),
    }
}

fn webp_has_animation(bytes: &[u8]) -> bool {
    bytes
        .windows(4)
        .any(|window| window == b"ANIM" || window == b"ANMF")
}

fn png_has_animation(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"acTL")
}

fn gif_frame_count(bytes: &[u8]) -> usize {
    if bytes.len() < 13 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return 0;
    }
    let packed = bytes[10];
    let mut idx = 13usize;
    if packed & 0x80 != 0 {
        let table_size = 3usize.saturating_mul(1usize << ((packed & 0x07) + 1));
        idx = idx.saturating_add(table_size);
    }
    let mut frames = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            0x2c => {
                frames = frames.saturating_add(1);
                idx = idx.saturating_add(10);
                if idx > bytes.len() {
                    break;
                }
                let descriptor_packed = bytes[idx - 1];
                if descriptor_packed & 0x80 != 0 {
                    let table_size =
                        3usize.saturating_mul(1usize << ((descriptor_packed & 0x07) + 1));
                    idx = idx.saturating_add(table_size);
                }
                if idx >= bytes.len() {
                    break;
                }
                idx = idx.saturating_add(1);
                idx = skip_gif_subblocks(bytes, idx);
            }
            0x21 => {
                idx = idx.saturating_add(2);
                idx = skip_gif_subblocks(bytes, idx);
            }
            0x3b => break,
            _ => break,
        }
        if frames > 1 {
            break;
        }
    }
    frames
}

fn skip_gif_subblocks(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() {
        let len = bytes[idx] as usize;
        idx = idx.saturating_add(1);
        if len == 0 {
            break;
        }
        idx = idx.saturating_add(len);
    }
    idx
}

fn benchmark_full_manifest_no_sqlite(
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) {
    let Some((canonical, _, _)) = benchmark_manifest_path_metadata(path, report, root) else {
        return;
    };
    let Some(bytes) = benchmark_read_full(&canonical, report, root) else {
        return;
    };
    let started = Instant::now();
    let _sha256 = sha256_bytes(&bytes);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_hash_ms += elapsed;
    root.timing.elapsed_hash_ms += elapsed;
    let Some(image) = benchmark_decode(&bytes, report, root) else {
        return;
    };
    let started = Instant::now();
    let (width, height) = image.dimensions();
    let _phash = phash64_hex(&image);
    let blur_score = image_blur_score(&image);
    let source_class = classify_source_path(&canonical);
    let _quality_flags = image_quality_flags(&canonical, width, height, blur_score, &source_class);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_phash_quality_ms += elapsed;
    root.timing.elapsed_phash_quality_ms += elapsed;
}

fn benchmark_fingerprint_no_sqlite(
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) {
    let Some((canonical, _, _)) = benchmark_manifest_path_metadata(path, report, root) else {
        return;
    };
    let Some(bytes) = benchmark_read_full(&canonical, report, root) else {
        return;
    };
    let started = Instant::now();
    let sha256 = sha256_bytes(&bytes);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_hash_ms += elapsed;
    root.timing.elapsed_hash_ms += elapsed;
    report.decode_count += 1;
    root.decode_count += 1;
    let started = Instant::now();
    let result = fingerprint_image_bytes_for_index(&canonical, &bytes, sha256, false);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_decode_ms += elapsed;
    root.timing.elapsed_decode_ms += elapsed;
    if result.is_err() {
        report.decode_errors += 1;
        root.decode_errors += 1;
        report.error_files += 1;
        root.error_files += 1;
    }
}

fn benchmark_turbojpeg_fingerprint_no_sqlite(
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) {
    let Some((canonical, _, _)) = benchmark_manifest_path_metadata(path, report, root) else {
        return;
    };
    let Some(bytes) = benchmark_read_full(&canonical, report, root) else {
        return;
    };
    let started = Instant::now();
    let sha256 = sha256_bytes(&bytes);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_hash_ms += elapsed;
    root.timing.elapsed_hash_ms += elapsed;
    report.decode_count += 1;
    root.decode_count += 1;
    let started = Instant::now();
    let result = fingerprint_image_bytes_for_turbojpeg_benchmark(&canonical, &bytes, sha256);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_decode_ms += elapsed;
    root.timing.elapsed_decode_ms += elapsed;
    if result.is_err() {
        report.decode_errors += 1;
        root.decode_errors += 1;
        report.error_files += 1;
        root.error_files += 1;
    }
}

fn benchmark_sqlite_batch_item(
    source_root: &Path,
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
    sqlite: Option<&mut BenchmarkSqliteWriter>,
) -> anyhow::Result<()> {
    let Some((canonical, file_size, mtime_unix)) =
        benchmark_manifest_path_metadata(path, report, root)
    else {
        return Ok(());
    };
    let path_text = canonical.to_string_lossy().to_string();
    let fingerprint = ImageFingerprint {
        sha256: benchmark_synthetic_sha(report.processed_files),
        phash: "0000000000000000".to_string(),
        phash_algo: PHASH_ALGO_V2.to_string(),
        width: 0,
        height: 0,
        blur_score: 0.0,
        blur_algo: BLUR_ALGO_V2.to_string(),
        quality_flags: "bench_sqlite".to_string(),
        source_class: classify_source_path(&canonical),
        detected_format: "BENCH".to_string(),
        has_alpha: false,
        orientation_applied: false,
        fingerprint_version: FINGERPRINT_VERSION_V2.to_string(),
        tile_hashes: Vec::new(),
    };
    let item = PendingIndexImage {
        source_root: source_root.to_path_buf(),
        path: canonical,
        path_text,
        file_size,
        mtime_unix,
        fingerprint,
        image: None,
        reuse_existing_asset: false,
    };
    match sqlite {
        Some(writer) => writer.push(item, report, root),
        None => Ok(()),
    }
}

fn benchmark_manifest_path_metadata(
    path: &Path,
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
) -> Option<(PathBuf, i64, i64)> {
    let started = Instant::now();
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_canonicalize_ms += elapsed;
    root.timing.elapsed_canonicalize_ms += elapsed;

    let started = Instant::now();
    let meta = fs::metadata(&canonical);
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_metadata_ms += elapsed;
    root.timing.elapsed_metadata_ms += elapsed;
    let meta = match meta {
        Ok(meta) => meta,
        Err(_) => {
            report.fileinfo_errors += 1;
            root.fileinfo_errors += 1;
            report.error_files += 1;
            root.error_files += 1;
            return None;
        }
    };
    let file_size = meta.len() as i64;
    let mtime_unix = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    Some((canonical, file_size, mtime_unix))
}

fn benchmark_synthetic_sha(value: usize) -> String {
    format!("{value:064x}")
}

fn platform_open_zero(path: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let handle = win32_open_zero_handle(path)?;
        unsafe {
            CloseHandle(handle);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs_file_open_for_host(path).map(|_| ()).map_err(Into::into)
    }
}

#[cfg(windows)]
fn win32_open_zero_handle(path: &Path) -> anyhow::Result<windows_sys::Win32::Foundation::HANDLE> {
    let wide = windows_path_wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        anyhow::bail!("CreateFileW failed for {}", path.display());
    }
    Ok(handle)
}

#[cfg(windows)]
fn windows_path_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
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

fn add_find_elapsed(
    report: &mut ManifestBenchmarkReport,
    root: &mut ManifestBenchmarkRootReport,
    started: Instant,
) {
    let elapsed = elapsed_ms_f64(started);
    report.timing.elapsed_find_ms += elapsed;
    root.timing.elapsed_find_ms += elapsed;
}

fn elapsed_ms_f64(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn elapsed_ms_u64(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn root_queue_has_more_images(states: &mut [RootScanState], active: &mut VecDeque<usize>) -> bool {
    while let Some(idx) = active.pop_front() {
        match states[idx].walker.next_with_budget(IMAGE_WALK_ENTRY_BUDGET) {
            ImageWalkStep::Image(_) | ImageWalkStep::Pending => return true,
            ImageWalkStep::Done => {}
        }
    }
    false
}

fn push_existing_dir(out: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() {
        let path = path.canonicalize().unwrap_or(path);
        if !out.iter().any(|item| item == &path) {
            out.push(path);
        }
    }
}

struct ImageFileWalker {
    pending_dirs: Vec<PathBuf>,
    current: Option<fs::ReadDir>,
    directories_seen: usize,
    files_seen: usize,
    errors: usize,
}

impl ImageFileWalker {
    fn new(root: &Path) -> Self {
        Self {
            pending_dirs: vec![root.to_path_buf()],
            current: None,
            directories_seen: 0,
            files_seen: 0,
            errors: 0,
        }
    }

    fn next_with_budget(&mut self, entry_budget: usize) -> ImageWalkStep {
        let mut visited = 0usize;
        loop {
            if visited >= entry_budget {
                return ImageWalkStep::Pending;
            }
            if self.current.is_none() {
                let Some(dir) = self.pending_dirs.pop() else {
                    return ImageWalkStep::Done;
                };
                visited += 1;
                self.directories_seen += 1;
                self.current = match fs::read_dir(dir) {
                    Ok(entries) => Some(entries),
                    Err(_) => {
                        self.errors += 1;
                        None
                    }
                };
                if self.current.is_none() {
                    continue;
                }
            }
            let entries = self.current.as_mut().expect("current read_dir exists");
            match entries.next() {
                Some(Ok(entry)) => {
                    visited += 1;
                    let Ok(ty) = entry.file_type() else {
                        self.errors += 1;
                        continue;
                    };
                    let path = entry.path();
                    if ty.is_dir() {
                        self.pending_dirs.push(path);
                    } else if ty.is_file() {
                        self.files_seen += 1;
                        if is_supported_image_path(&path) {
                            return ImageWalkStep::Image(path);
                        }
                    }
                }
                Some(Err(_)) => {
                    visited += 1;
                    self.errors += 1;
                }
                None => self.current = None,
            }
        }
    }
}

enum ImageWalkStep {
    Image(PathBuf),
    Pending,
    Done,
}

impl Iterator for ImageFileWalker {
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.next_with_budget(usize::MAX) {
                ImageWalkStep::Image(path) => return Some(path),
                ImageWalkStep::Pending => continue,
                ImageWalkStep::Done => return None,
            }
        }
    }
}

fn is_supported_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" | "tif" | "tiff"
    )
}

fn required_embedding_keys(options: &BuildOptions) -> anyhow::Result<Vec<(String, String)>> {
    let mut required = Vec::new();
    let embedding_kind = embedding_kind()?;
    if embedding_kind.wants_clip() {
        if let Some(model_key) = required_clip_model_key(options)? {
            required.push(("clip".to_string(), model_key));
        }
    }
    if embedding_kind.wants_sscd() {
        if let Some(model_key) = required_sscd_model_key(options) {
            required.push(("sscd".to_string(), model_key));
        }
    }
    Ok(required)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmbeddingKind {
    Both,
    Clip,
    Sscd,
}

impl EmbeddingKind {
    fn wants_clip(self) -> bool {
        matches!(self, Self::Both | Self::Clip)
    }

    fn wants_sscd(self) -> bool {
        matches!(self, Self::Both | Self::Sscd)
    }
}

fn embedding_kind() -> anyhow::Result<EmbeddingKind> {
    let Some(value) = env::var(EMBEDDING_KIND_ENV)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
    else {
        return Ok(EmbeddingKind::Both);
    };
    match value.as_str() {
        "both" | "all" | "default" => Ok(EmbeddingKind::Both),
        "clip" | "clip-only" | "clip_only" => Ok(EmbeddingKind::Clip),
        "sscd" | "sscd-only" | "sscd_only" => Ok(EmbeddingKind::Sscd),
        _ => anyhow::bail!("{EMBEDDING_KIND_ENV}={value}; expected both, clip, or sscd"),
    }
}

#[cfg(feature = "image-index-clip")]
fn required_clip_model_key(options: &BuildOptions) -> anyhow::Result<Option<String>> {
    let Some(model_dir) = resolve_clip_model_dir(options) else {
        return Ok(None);
    };
    if !clip_model_dir_complete(&model_dir) {
        anyhow::bail!("incomplete CLIP model dir: {}", model_dir.display());
    }
    Ok(Some(clip_model_key(&options.clip_model, &model_dir)))
}

#[cfg(not(feature = "image-index-clip"))]
fn required_clip_model_key(_options: &BuildOptions) -> anyhow::Result<Option<String>> {
    Ok(None)
}

#[cfg(feature = "image-index-sscd")]
fn required_sscd_model_key(options: &BuildOptions) -> Option<String> {
    resolve_sscd_model_path(options).map(|path| sscd_model_key(&options.sscd_model, &path))
}

#[cfg(not(feature = "image-index-sscd"))]
fn required_sscd_model_key(_options: &BuildOptions) -> Option<String> {
    None
}

#[derive(Default)]
struct FlushStats {
    indexed: usize,
    errors: usize,
}

#[derive(Default)]
struct PrepareBatchStats {
    items: Vec<PendingIndexImage>,
    errors: Vec<(PathBuf, String)>,
    profile: EmbeddingProfileReport,
    workers: usize,
}

struct PreparePoolJob {
    batch_index: usize,
    asset: PendingEmbeddingAsset,
    scaled_jpeg_decode_target: Option<u32>,
    result_sender: Sender<PreparePoolResult>,
}

struct PreparePoolResult {
    batch_index: usize,
    path: PathBuf,
    item: anyhow::Result<PendingIndexImage>,
    profile: EmbeddingProfileReport,
}

struct EmbeddingPreparePool {
    sender: Option<Sender<PreparePoolJob>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

struct PrepareBatchHandle {
    paths: Vec<String>,
    receiver: Receiver<PreparePoolResult>,
    jobs: usize,
    workers: usize,
}

impl EmbeddingPreparePool {
    fn start(requested_workers: usize) -> anyhow::Result<Self> {
        let worker_count = requested_workers.clamp(1, MAX_MANIFEST_WORKERS);
        let (sender, receiver) = unbounded::<PreparePoolJob>();
        let mut workers = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let receiver = receiver.clone();
            workers.push(
                std::thread::Builder::new()
                    .name(format!("image-prepare-{index}"))
                    .spawn(move || prepare_pool_worker(receiver))?,
            );
        }
        Ok(Self {
            sender: Some(sender),
            workers,
        })
    }

    fn submit(
        &self,
        backlog: Vec<PendingEmbeddingAsset>,
        scaled_jpeg_decode_target: Option<u32>,
    ) -> anyhow::Result<Option<PrepareBatchHandle>> {
        if backlog.is_empty() {
            return Ok(None);
        }
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("embedding prepare pool is stopped"))?;
        let paths = backlog
            .iter()
            .map(|asset| asset.path.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        let jobs = backlog.len();
        let workers = self.workers.len().min(jobs);
        let (result_sender, receiver) = unbounded();
        for (batch_index, asset) in backlog.into_iter().enumerate() {
            sender
                .send(PreparePoolJob {
                    batch_index,
                    asset,
                    scaled_jpeg_decode_target,
                    result_sender: result_sender.clone(),
                })
                .map_err(|_| anyhow::anyhow!("embedding prepare pool stopped"))?;
        }
        drop(result_sender);
        Ok(Some(PrepareBatchHandle {
            paths,
            receiver,
            jobs,
            workers,
        }))
    }

    fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

impl Drop for EmbeddingPreparePool {
    fn drop(&mut self) {
        self.sender.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn prepare_pool_worker(receiver: Receiver<PreparePoolJob>) {
    while let Ok(job) = receiver.recv() {
        let path = job.asset.path.clone();
        let mut profiler = EmbeddingProfiler::default();
        profiler.report.prepare_jobs = 1;
        let item = prepare_embedding_asset(
            job.asset,
            job.scaled_jpeg_decode_target,
            Some(&mut profiler),
        );
        if item.is_err() {
            profiler.report.prepare_errors = 1;
        }
        let _ = job.result_sender.send(PreparePoolResult {
            batch_index: job.batch_index,
            path,
            item,
            profile: profiler.finish(),
        });
    }
}

struct PreparedEmbeddingBatch {
    stats: PrepareBatchStats,
    inputs: Option<PreprocessedEmbeddingInputs>,
    input_prefetch_ms: f64,
    input_prefetch_batches: usize,
}

struct EmbeddingWriteBatch {
    items: Vec<PendingIndexImage>,
    clip_model_key: Option<String>,
    clip_vectors: Option<Vec<Vec<f32>>>,
    sscd_model_key: Option<String>,
    sscd_vectors: Option<Vec<Vec<f32>>>,
    indexed_at: String,
}

enum EmbeddingWriterMessage {
    Batch(EmbeddingWriteBatch),
    Finish,
}

#[derive(Default)]
struct EmbeddingWriterStats {
    indexed: usize,
    sqlite_ms: f64,
}

struct EmbeddingWriter {
    sender: mpsc::SyncSender<EmbeddingWriterMessage>,
    join: Option<std::thread::JoinHandle<anyhow::Result<EmbeddingWriterStats>>>,
}

#[derive(Default)]
struct PreprocessedEmbeddingInputs {
    #[cfg(feature = "image-index-clip")]
    clip: Option<anyhow::Result<open_clip_inference::vision::VisionPreprocessedBatch>>,
    #[cfg(feature = "image-index-sscd")]
    sscd: Option<anyhow::Result<(SscdPreprocessedBatch, SscdPreprocessProfile)>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmbedInputPrefetchMode {
    None,
    Clip,
    Sscd,
    All,
}

impl EmbedInputPrefetchMode {
    fn is_enabled(self) -> bool {
        !matches!(self, Self::None)
    }

    #[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
    fn wants_clip(self) -> bool {
        matches!(self, Self::Clip | Self::All)
    }

    #[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
    fn wants_sscd(self) -> bool {
        matches!(self, Self::Sscd | Self::All)
    }

    #[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
    fn status_value(self) -> &'static str {
        match self {
            Self::None => "0",
            Self::Clip => "clip",
            Self::Sscd => "sscd",
            Self::All => "all",
        }
    }
}

impl EmbeddingWriter {
    fn start(manifest: PathBuf) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<EmbeddingWriterMessage>(2);
        let join = std::thread::spawn(move || {
            let mut con = Connection::open(&manifest)?;
            init_schema(&con)?;
            let mut stats = EmbeddingWriterStats::default();
            while let Ok(message) = receiver.recv() {
                match message {
                    EmbeddingWriterMessage::Batch(batch) => {
                        let (indexed, sqlite_ms) = write_embedding_batch(&mut con, batch)?;
                        stats.indexed += indexed;
                        stats.sqlite_ms += sqlite_ms;
                    }
                    EmbeddingWriterMessage::Finish => break,
                }
            }
            Ok(stats)
        });
        Self {
            sender,
            join: Some(join),
        }
    }

    fn enqueue(&self, batch: EmbeddingWriteBatch) -> anyhow::Result<()> {
        self.sender
            .send(EmbeddingWriterMessage::Batch(batch))
            .map_err(|_| anyhow::anyhow!("embedding SQLite writer stopped before accepting batch"))
    }

    fn finish(mut self) -> anyhow::Result<EmbeddingWriterStats> {
        let _ = self.sender.send(EmbeddingWriterMessage::Finish);
        self.join
            .take()
            .ok_or_else(|| anyhow::anyhow!("embedding SQLite writer was already joined"))?
            .join()
            .map_err(|_| anyhow::anyhow!("embedding SQLite writer panicked"))?
    }
}

impl Drop for EmbeddingWriter {
    fn drop(&mut self) {
        let _ = self.sender.send(EmbeddingWriterMessage::Finish);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn async_embedding_writer_enabled(execution_provider: &str) -> anyhow::Result<bool> {
    if let Some(value) = bool_env(ASYNC_SQLITE_WRITER_ENV)? {
        return Ok(value);
    }
    let _ = execution_provider;
    Ok(false)
}

#[cfg(feature = "image-index-sscd")]
enum SscdPreprocessedBatch {
    F32(ndarray::Array4<f32>),
    F16(ndarray::Array4<half::f16>),
}

#[cfg(feature = "image-index-sscd")]
#[derive(Clone, Copy, Debug, Default)]
struct SscdPreprocessProfile {
    wall_ms: f64,
    resize_cpu_ms: f64,
    normalize_cpu_ms: f64,
    workers: usize,
}

#[cfg(feature = "image-index-sscd")]
impl SscdPreprocessProfile {
    fn add_cpu(self, other: Self) -> Self {
        Self {
            wall_ms: 0.0,
            resize_cpu_ms: self.resize_cpu_ms + other.resize_cpu_ms,
            normalize_cpu_ms: self.normalize_cpu_ms + other.normalize_cpu_ms,
            workers: self.workers.max(other.workers),
        }
    }

    fn with_wall_and_workers(self, wall_ms: f64, workers: usize) -> Self {
        Self {
            wall_ms,
            workers,
            ..self
        }
    }
}

fn scaled_jpeg_decode_target(want_clip: bool, want_sscd: bool) -> anyhow::Result<Option<u32>> {
    #[cfg(windows)]
    {
        #[cfg(feature = "image-index-clip")]
        if want_clip
            && env::var_os("QQ_ANALYZER_TURBOJPEG_DLL").is_some()
            && bool_env(CLIP_SCALED_JPEG_DECODE_ENV)?.unwrap_or(false)
        {
            let target = positive_u32_env(CLIP_SCALED_JPEG_MIN_EDGE_ENV)?.unwrap_or(384);
            return Ok(Some(target));
        }
        #[cfg(feature = "image-index-sscd")]
        if want_sscd
            && !want_clip
            && env::var_os("QQ_ANALYZER_TURBOJPEG_DLL").is_some()
            && bool_env(SSCD_SCALED_JPEG_DECODE_ENV)?.unwrap_or(false)
        {
            let target =
                positive_u32_env(SSCD_SCALED_JPEG_MIN_EDGE_ENV)?.unwrap_or(SSCD_INPUT_SIZE * 3 / 2);
            return Ok(Some(target));
        }
    }
    let _ = (want_clip, want_sscd);
    Ok(None)
}

#[cfg(windows)]
fn positive_u32_env(name: &str) -> anyhow::Result<Option<u32>> {
    let Some(value) = env::var(name).ok() else {
        return Ok(None);
    };
    let parsed = value
        .trim()
        .parse::<u32>()
        .map_err(|err| anyhow::anyhow!("{name}={value}; expected a positive integer: {err}"))?;
    if parsed == 0 {
        anyhow::bail!("{name} must be greater than zero");
    }
    Ok(Some(parsed))
}

fn build_embedding_stage(
    con: &mut Connection,
    options: &BuildOptions,
    required_embeddings: &[(String, String)],
    report: &mut BuildReport,
) -> anyhow::Result<()> {
    let trace_started = Instant::now();
    trace_timing(trace_started, "embedding_stage:start");
    if required_embeddings.is_empty() {
        return Ok(());
    }
    let want_clip = required_embeddings.iter().any(|(kind, _)| kind == "clip");
    let want_sscd = required_embeddings.iter().any(|(kind, _)| kind == "sscd");
    let scaled_jpeg_decode_target = scaled_jpeg_decode_target(want_clip, want_sscd)?;
    let input_prefetch_mode = embed_input_prefetch_mode();
    #[cfg(feature = "image-index-sscd")]
    let prefetches_sscd = input_prefetch_mode.wants_sscd();
    #[cfg(not(feature = "image-index-sscd"))]
    let prefetches_sscd = false;
    let default_prepare_depth = if scaled_jpeg_decode_target.is_some() && prefetches_sscd {
        2
    } else {
        1
    };
    let prepare_depth = prepare_prefetch_depth(default_prepare_depth)?;
    let prepare_pool = EmbeddingPreparePool::start(options.manifest_workers)?;
    let mut clip_vision = None;
    let mut sscd = None;
    let mut pending = Vec::with_capacity(options.clip_batch_size.max(1));
    let mut next_prepare: Option<PrepareBatchHandle> = None;
    let mut lookahead_prepare: Option<PrepareBatchHandle> = None;
    let mut next_ready: Option<PreparedEmbeddingBatch> = None;
    let mut processed = 0usize;
    let mut iteration = 0usize;
    let mut profiler = EmbeddingProfiler::default();
    profiler.report.prepare_pool_threads_started = prepare_pool.worker_count();
    profiler.report.prepare_pool_persistent = true;
    profiler.report.prepare_dynamic_scheduling = true;
    profiler.report.prepare_prefetch_depth = prepare_depth;
    let manifest = options
        .manifest_path
        .clone()
        .unwrap_or_else(|| default_manifest_path(&options.root, &options.account));
    let mut async_writer = if async_embedding_writer_enabled(&options.execution_provider)? {
        Some(EmbeddingWriter::start(manifest))
    } else {
        None
    };
    trace_timing(
        trace_started,
        &format!("embedding_stage:reuse_start processed={processed}"),
    );
    let started = Instant::now();
    let reused = reuse_duplicate_embeddings(con, required_embeddings)?;
    EmbeddingProfiler::add_ms(&mut profiler.report.reuse_ms, started);
    trace_timing(
        trace_started,
        &format!("embedding_stage:reuse_done reused={reused}"),
    );
    report.reused_embedding_files += reused;
    processed = processed.saturating_add(reused);
    let pending_limit = options.max_files.saturating_sub(processed);
    let mut pending_backlog = if pending_limit == 0 {
        VecDeque::new()
    } else {
        trace_timing(
            trace_started,
            &format!("embedding_stage:pending_start limit={pending_limit}"),
        );
        let started = Instant::now();
        let backlog =
            pending_embedding_paths_excluding(con, required_embeddings, pending_limit, &[])?;
        EmbeddingProfiler::add_ms(&mut profiler.report.pending_query_ms, started);
        trace_timing(
            trace_started,
            &format!("embedding_stage:pending_done queued={}", backlog.len()),
        );
        VecDeque::from(backlog)
    };
    if !pending_backlog.is_empty() {
        let batch_limit = options
            .clip_batch_size
            .max(1)
            .min(options.max_files.saturating_sub(processed).max(1));
        next_prepare = prepare_pool.submit(
            take_embedding_backlog_batch(&mut pending_backlog, batch_limit),
            scaled_jpeg_decode_target,
        )?;
        if prepare_depth > 1 && !pending_backlog.is_empty() {
            lookahead_prepare = prepare_pool.submit(
                take_embedding_backlog_batch(&mut pending_backlog, batch_limit),
                scaled_jpeg_decode_target,
            )?;
        }
        trace_timing(
            trace_started,
            &format!(
                "embedding_stage:initial_pending_scheduled={}",
                next_prepare
                    .as_ref()
                    .map(|handle| handle.paths.len())
                    .unwrap_or(0)
            ),
        );
        if next_prepare.is_some() {
            trace_timing(trace_started, "embedding_stage:runtime_warmup_start");
            ensure_embedding_runtimes(
                options,
                want_clip,
                want_sscd,
                &mut clip_vision,
                &mut sscd,
                &mut profiler,
            )?;
            trace_timing(trace_started, "embedding_stage:runtime_warmup_done");
        }
    }
    loop {
        if next_prepare.is_none() {
            next_prepare = lookahead_prepare.take();
        }
        if next_prepare.is_none() && next_ready.is_none() {
            break;
        }
        iteration += 1;
        profiler.report.iterations += 1;
        trace_timing(
            trace_started,
            &format!("embedding_stage:iter{iteration}:prepare_start"),
        );
        let current_batch = if let Some(batch) = next_ready.take() {
            batch
        } else {
            let current_prepare = next_prepare
                .take()
                .ok_or_else(|| anyhow::anyhow!("missing scheduled embedding prepare batch"))?;
            let started = Instant::now();
            let batch = join_prepare_batch(current_prepare)?;
            EmbeddingProfiler::add_ms(&mut profiler.report.prepare_ms, started);
            batch
        };
        profiler.report.prefetch_embed_input_batches += current_batch.input_prefetch_batches;
        profiler.report.prefetch_embed_input_ms += current_batch.input_prefetch_ms;
        let prepared = current_batch.stats;
        let current_inputs = current_batch.inputs;
        profiler.report.prepare_workers = profiler.report.prepare_workers.max(prepared.workers);
        profiler.merge_prepare(prepared.profile);
        let prepare_errors = prepared.errors.len();
        pending.extend(prepared.items);
        report.error_files += prepare_errors;
        for (path, error) in prepared.errors {
            let error_started = Instant::now();
            record_path_error(con, &path.to_string_lossy(), &error)?;
            EmbeddingProfiler::add_ms(&mut profiler.report.record_error_ms, error_started);
        }
        trace_timing(
            trace_started,
            &format!(
                "embedding_stage:iter{iteration}:prepare_done pending={} prepare_errors={prepare_errors}",
                pending.len()
            ),
        );
        let mut scheduled = pending.len()
            + prepare_errors
            + next_prepare.as_ref().map(|handle| handle.jobs).unwrap_or(0)
            + lookahead_prepare
                .as_ref()
                .map(|handle| handle.jobs)
                .unwrap_or(0);
        if next_prepare.is_none()
            && processed.saturating_add(scheduled) < options.max_files
            && !pending_backlog.is_empty()
        {
            let batch_limit = options.clip_batch_size.max(1).min(
                options
                    .max_files
                    .saturating_sub(processed + scheduled)
                    .max(1),
            );
            trace_timing(
                trace_started,
                &format!("embedding_stage:iter{iteration}:prefetch_start limit={batch_limit}"),
            );
            next_prepare = prepare_pool.submit(
                take_embedding_backlog_batch(&mut pending_backlog, batch_limit),
                scaled_jpeg_decode_target,
            )?;
            trace_timing(
                trace_started,
                &format!(
                    "embedding_stage:iter{iteration}:prefetch_done scheduled={}",
                    next_prepare
                        .as_ref()
                        .map(|handle| handle.paths.len())
                        .unwrap_or(0)
                ),
            );
            scheduled += next_prepare.as_ref().map(|handle| handle.jobs).unwrap_or(0);
        }
        if prepare_depth > 1
            && lookahead_prepare.is_none()
            && processed.saturating_add(scheduled) < options.max_files
            && !pending_backlog.is_empty()
        {
            let batch_limit = options.clip_batch_size.max(1).min(
                options
                    .max_files
                    .saturating_sub(processed + scheduled)
                    .max(1),
            );
            lookahead_prepare = prepare_pool.submit(
                take_embedding_backlog_batch(&mut pending_backlog, batch_limit),
                scaled_jpeg_decode_target,
            )?;
            trace_timing(
                trace_started,
                &format!(
                    "embedding_stage:iter{iteration}:lookahead_scheduled={}",
                    lookahead_prepare
                        .as_ref()
                        .map(|handle| handle.paths.len())
                        .unwrap_or(0)
                ),
            );
        }
        trace_timing(
            trace_started,
            &format!("embedding_stage:iter{iteration}:flush_start"),
        );
        let stats = if input_prefetch_mode.is_enabled()
            && next_prepare.is_some()
            && (want_clip || want_sscd)
        {
            ensure_embedding_runtimes(
                options,
                want_clip,
                want_sscd,
                &mut clip_vision,
                &mut sscd,
                &mut profiler,
            )?;
            let next_handle = next_prepare
                .take()
                .ok_or_else(|| anyhow::anyhow!("missing scheduled embedding prepare batch"))?;
            let clip_ref = clip_vision.as_ref();
            let sscd_ref = sscd.as_ref();
            let mut current_inputs = current_inputs;
            let (stats, ready) = std::thread::scope(
                |scope| -> anyhow::Result<(FlushStats, PreparedEmbeddingBatch)> {
                    let prefetch = scope.spawn(move || {
                        join_prepare_batch_with_inputs(
                            next_handle,
                            clip_ref,
                            sscd_ref,
                            input_prefetch_mode,
                        )
                    });
                    let started = Instant::now();
                    let stats = flush_index_batch(
                        con,
                        clip_vision.as_ref(),
                        sscd.as_ref(),
                        &mut pending,
                        current_inputs.take(),
                        async_writer.as_ref(),
                        &mut profiler,
                    )?;
                    EmbeddingProfiler::add_ms(&mut profiler.report.flush_ms, started);
                    let started = Instant::now();
                    let ready = prefetch.join().map_err(|_| {
                        anyhow::anyhow!("embedding input prefetch worker panicked")
                    })??;
                    EmbeddingProfiler::add_ms(
                        &mut profiler.report.prefetch_embed_input_wait_ms,
                        started,
                    );
                    Ok((stats, ready))
                },
            )?;
            next_ready = Some(ready);
            stats
        } else {
            let started = Instant::now();
            let stats = flush_index_batch_lazy(
                con,
                options,
                want_clip,
                want_sscd,
                &mut clip_vision,
                &mut sscd,
                &mut pending,
                current_inputs,
                async_writer.as_ref(),
                &mut profiler,
            )?;
            EmbeddingProfiler::add_ms(&mut profiler.report.flush_ms, started);
            stats
        };
        trace_timing(
            trace_started,
            &format!(
                "embedding_stage:iter{iteration}:flush_done indexed={} errors={}",
                stats.indexed, stats.errors
            ),
        );
        if stats.indexed == 0 && stats.errors == 0 && prepare_errors == 0 {
            break;
        }
        report.embedded_files += stats.indexed;
        report.error_files += stats.errors;
        processed = processed.saturating_add(stats.indexed + stats.errors + prepare_errors);
    }
    if let Some(writer) = async_writer.take() {
        let writer_stats = writer.finish()?;
        profiler.report.sqlite_ms += writer_stats.sqlite_ms;
    }
    if let Some(runtime) = clip_vision.as_ref() {
        profiler.report.clip_ort_profile_path = runtime.ort_profile_path();
        match runtime.finish_ort_profiling() {
            Ok(path) => profiler.report.clip_ort_profile_output_path = path,
            Err(err) => profiler.report.clip_ort_profile_error = Some(err.to_string()),
        }
    }
    if let Some(runtime) = sscd.as_ref() {
        profiler.report.sscd_ort_profile_path = runtime.ort_profile_path();
        match runtime.finish_ort_profiling() {
            Ok(path) => profiler.report.sscd_ort_profile_output_path = path,
            Err(err) => profiler.report.sscd_ort_profile_error = Some(err.to_string()),
        }
    }
    report.embedding_profile = Some(profiler.finish());
    trace_timing(trace_started, "embedding_stage:done");
    Ok(())
}

fn pending_embedding_paths_excluding(
    con: &Connection,
    required_embeddings: &[(String, String)],
    limit: usize,
    excluded_paths: &[String],
) -> anyhow::Result<Vec<PendingEmbeddingAsset>> {
    if required_embeddings.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut missing = Vec::with_capacity(required_embeddings.len());
    let mut params = Vec::with_capacity(required_embeddings.len() * 2 + excluded_paths.len() + 1);
    for (kind, model) in required_embeddings {
        missing.push(
            r#"
            not exists (
                select 1
                from image_embeddings e
                where e.path=a.path
                  and e.kind=?
                  and e.model=?
                  and length(e.sketch64_hex)=16
                  and e.bucket12 is not null
                  and e.bucket12>=0
            )
            "#,
        );
        params.push(rusqlite::types::Value::Text(kind.clone()));
        params.push(rusqlite::types::Value::Text(model.clone()));
    }
    let excluded_clause = if excluded_paths.is_empty() {
        String::new()
    } else {
        for path in excluded_paths {
            params.push(rusqlite::types::Value::Text(path.clone()));
        }
        format!(
            "and a.path not in ({})",
            std::iter::repeat_n("?", excluded_paths.len())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    params.push(rusqlite::types::Value::Integer(limit as i64));
    let sql = format!(
        r#"
        select
          a.path,
          a.source_root,
          a.file_size,
          a.mtime_unix,
          a.sha256_hex,
          a.phash_hex,
          a.phash_algo,
          a.width,
          a.height,
          a.blur_score,
          a.blur_algo,
          a.quality_flags,
          a.source_class,
          a.detected_format,
          a.has_alpha,
          a.orientation_applied,
          a.fingerprint_version
        from image_assets a
        where a.stale=0
          and (a.error is null or a.error='')
          and ({})
          {}
        order by
          case
            when a.source_class in ('chat_pic', 'file_recv', 'legacy_image', 'image')
             and instr(',' || a.quality_flags || ',', ',blurry,')=0
             and instr(',' || a.quality_flags || ',', ',small,')=0
             and instr(',' || a.quality_flags || ',', ',tiny,')=0 then 0
            when a.source_class in ('chat_pic', 'file_recv', 'legacy_image', 'image') then 1
            when instr(',' || a.quality_flags || ',', ',thumbnail_like,')>0 then 3
            else 2
          end,
          a.path
        limit ?
        "#,
        missing.join(" or "),
        excluded_clause
    );
    let mut stmt = con.prepare_cached(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        let path = PathBuf::from(row.get::<_, String>(0)?);
        let source_root = PathBuf::from(row.get::<_, String>(1)?);
        let file_size = row.get::<_, i64>(2)?;
        let mtime_unix = row.get::<_, i64>(3)?;
        let fingerprint = existing_asset_fingerprint_from_row(row, 4)?;
        Ok(PendingEmbeddingAsset {
            path,
            source_root,
            file_size,
            mtime_unix,
            fingerprint,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn existing_asset_fingerprint_from_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<Option<ImageFingerprint>> {
    let sha256 = row.get::<_, String>(offset)?;
    let phash = row
        .get::<_, Option<String>>(offset + 1)?
        .unwrap_or_default();
    let phash_algo = row.get::<_, String>(offset + 2)?;
    let width = row.get::<_, Option<i64>>(offset + 3)?.unwrap_or(0);
    let height = row.get::<_, Option<i64>>(offset + 4)?.unwrap_or(0);
    let blur_score = row.get::<_, Option<f64>>(offset + 5)?.unwrap_or(0.0);
    let blur_algo = row.get::<_, String>(offset + 6)?;
    let quality_flags = row.get::<_, String>(offset + 7)?;
    let source_class = row.get::<_, String>(offset + 8)?;
    let detected_format = row.get::<_, String>(offset + 9)?;
    let has_alpha = row.get::<_, i64>(offset + 10)? != 0;
    let orientation_applied = row.get::<_, i64>(offset + 11)? != 0;
    let fingerprint_version = row.get::<_, String>(offset + 12)?;
    if sha256.is_empty()
        || phash.len() != 16
        || phash_algo != PHASH_ALGO_V2
        || width <= 0
        || height <= 0
        || blur_algo != BLUR_ALGO_V2
        || source_class.is_empty()
        || fingerprint_version != FINGERPRINT_VERSION_V2
    {
        return Ok(None);
    }
    Ok(Some(ImageFingerprint {
        sha256,
        phash,
        phash_algo,
        width: width as u32,
        height: height as u32,
        blur_score,
        blur_algo,
        quality_flags,
        source_class,
        detected_format,
        has_alpha,
        orientation_applied,
        fingerprint_version,
        tile_hashes: Vec::new(),
    }))
}

fn take_embedding_backlog_batch(
    backlog: &mut VecDeque<PendingEmbeddingAsset>,
    limit: usize,
) -> Vec<PendingEmbeddingAsset> {
    let take = limit.min(backlog.len());
    backlog.drain(..take).collect()
}

fn join_prepare_batch(handle: PrepareBatchHandle) -> anyhow::Result<PreparedEmbeddingBatch> {
    let PrepareBatchHandle {
        paths,
        receiver,
        jobs,
        workers,
    } = handle;
    let mut ordered = (0..jobs).map(|_| None).collect::<Vec<_>>();
    for _ in 0..jobs {
        let result = receiver.recv().map_err(|_| {
            anyhow::anyhow!(
                "embedding prepare pool stopped after scheduling {} paths",
                paths.len()
            )
        })?;
        let index = result.batch_index;
        let slot = ordered
            .get_mut(index)
            .ok_or_else(|| anyhow::anyhow!("invalid embedding prepare result index {index}"))?;
        if slot.is_some() {
            anyhow::bail!("duplicate embedding prepare result index {index}");
        }
        *slot = Some(result);
    }
    let mut stats = PrepareBatchStats {
        workers,
        ..PrepareBatchStats::default()
    };
    for result in ordered.into_iter().flatten() {
        merge_prepare_profile(&mut stats.profile, result.profile);
        match result.item {
            Ok(item) => stats.items.push(item),
            Err(err) => stats.errors.push((result.path, err.to_string())),
        }
    }
    Ok(PreparedEmbeddingBatch {
        stats,
        inputs: None,
        input_prefetch_ms: 0.0,
        input_prefetch_batches: 0,
    })
}

#[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
fn join_prepare_batch_with_inputs(
    handle: PrepareBatchHandle,
    clip_vision: Option<&ClipVisionRuntime>,
    sscd: Option<&SscdRuntime>,
    prefetch_mode: EmbedInputPrefetchMode,
) -> anyhow::Result<PreparedEmbeddingBatch> {
    let mut batch = join_prepare_batch(handle)?;
    let started = Instant::now();
    batch.inputs = precompute_embedding_inputs(&mut batch.stats, clip_vision, sscd, prefetch_mode);
    if batch.inputs.is_some() {
        batch.input_prefetch_ms = elapsed_ms_f64(started);
        batch.input_prefetch_batches = 1;
    }
    Ok(batch)
}

#[cfg(not(any(feature = "image-index-clip", feature = "image-index-sscd")))]
fn join_prepare_batch_with_inputs(
    handle: PrepareBatchHandle,
    _clip_vision: Option<&ClipVisionRuntime>,
    _sscd: Option<&SscdRuntime>,
    _prefetch_mode: EmbedInputPrefetchMode,
) -> anyhow::Result<PreparedEmbeddingBatch> {
    join_prepare_batch(handle)
}

#[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
fn precompute_embedding_inputs(
    stats: &mut PrepareBatchStats,
    clip_vision: Option<&ClipVisionRuntime>,
    sscd: Option<&SscdRuntime>,
    prefetch_mode: EmbedInputPrefetchMode,
) -> Option<PreprocessedEmbeddingInputs> {
    let wants_clip = prefetch_mode.wants_clip() && clip_vision.is_some();
    let wants_sscd = prefetch_mode.wants_sscd() && sscd.is_some();
    if stats.items.is_empty() || (!wants_clip && !wants_sscd) {
        return None;
    }
    let mut images = Vec::with_capacity(stats.items.len());
    for item in &mut stats.items {
        let Some(image) = item.image.take() else {
            for (item, image) in stats.items.iter_mut().zip(images) {
                item.image = Some(image);
            }
            return None;
        };
        images.push(image);
    }

    let mut inputs = PreprocessedEmbeddingInputs::default();
    #[cfg(feature = "image-index-clip")]
    {
        if wants_clip {
            let runtime = clip_vision.expect("checked CLIP runtime for input prefetch");
            inputs.clip = Some(runtime.preprocess_images_profiled(&images));
        }
    }
    #[cfg(feature = "image-index-sscd")]
    {
        if wants_sscd {
            let runtime = sscd.expect("checked SSCD runtime for input prefetch");
            inputs.sscd = Some(runtime.tensor_batch_size_for(images.len()).and_then(
                |tensor_batch_size| {
                    sscd_preprocess_batch_timed(
                        &images,
                        runtime.input_tensor_type,
                        tensor_batch_size,
                    )
                },
            ));
        }
    }

    for (item, image) in stats.items.iter_mut().zip(images) {
        item.image = Some(image);
    }
    Some(inputs)
}

fn embed_input_prefetch_mode() -> EmbedInputPrefetchMode {
    env::var(PREFETCH_EMBED_INPUTS_ENV)
        .ok()
        .map(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "all" => EmbedInputPrefetchMode::All,
            "clip" | "clip-only" | "clip_only" => EmbedInputPrefetchMode::Clip,
            "sscd" | "sscd-only" | "sscd_only" => EmbedInputPrefetchMode::Sscd,
            _ => EmbedInputPrefetchMode::None,
        })
        .unwrap_or(EmbedInputPrefetchMode::None)
}

fn prepare_prefetch_depth(default_depth: usize) -> anyhow::Result<usize> {
    let Some(value) = env::var(PREPARE_PREFETCH_DEPTH_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(default_depth);
    };
    let depth = value.parse::<usize>().map_err(|err| {
        anyhow::anyhow!(
            "{PREPARE_PREFETCH_DEPTH_ENV}={value}; expected an integer from 1 to 2: {err}"
        )
    })?;
    if !(1..=2).contains(&depth) {
        anyhow::bail!("{PREPARE_PREFETCH_DEPTH_ENV}={depth}; expected 1 or 2");
    }
    Ok(depth)
}

fn reuse_duplicate_embeddings(
    con: &Connection,
    required_embeddings: &[(String, String)],
) -> anyhow::Result<usize> {
    let has_exact_hashes: i64 = con.query_row(
        r#"
        select exists(
            select 1
            from image_assets
            where stale=0 and sha256_hex>''
            limit 1
        )
        "#,
        [],
        |row| row.get(0),
    )?;
    if has_exact_hashes == 0 {
        return Ok(0);
    }
    let mut total = 0usize;
    for (kind, model) in required_embeddings {
        let has_missing_exact_hashes: i64 = con.query_row(
            r#"
            select exists(
                select 1
                from image_assets missing
                where missing.stale=0
                  and (missing.error is null or missing.error='')
                  and missing.sha256_hex>''
                  and not exists (
                      select 1
                      from image_embeddings existing
                      where existing.path=missing.path
                        and existing.kind=?1
                        and existing.model=?2
                        and length(existing.sketch64_hex)=16
                        and existing.bucket12 is not null
                        and existing.bucket12>=0
                  )
                limit 1
            )
            "#,
            params![kind, model],
            |row| row.get(0),
        )?;
        if has_missing_exact_hashes == 0 {
            continue;
        }
        total += con.execute(
            r#"
            insert or ignore into image_embeddings
                (path, kind, model, dim, vec, sketch64_hex, bucket12, updated_at)
            select missing.path, e.kind, e.model, e.dim, e.vec, e.sketch64_hex, e.bucket12, e.updated_at
            from image_assets missing
            join image_assets source
              on source.sha256_hex=missing.sha256_hex
             and source.path<>missing.path
              and source.stale=0
            join image_embeddings e
              on e.path=source.path
             and e.kind=?1
             and e.model=?2
             and length(e.sketch64_hex)=16
             and e.bucket12 is not null
             and e.bucket12>=0
            where missing.stale=0
              and (missing.error is null or missing.error='')
              and missing.sha256_hex>''
              and not exists (
                  select 1
                  from image_embeddings existing
                  where existing.path=missing.path
                    and existing.kind=?3
                    and existing.model=?4
                    and length(existing.sketch64_hex)=16
                    and existing.bucket12 is not null
                    and existing.bucket12>=0
              )
            "#,
            params![kind, model, kind, model],
        )?;
    }
    Ok(total)
}

fn prepare_embedding_asset(
    asset: PendingEmbeddingAsset,
    scaled_jpeg_decode_target: Option<u32>,
    mut profiler: Option<&mut EmbeddingProfiler>,
) -> anyhow::Result<PendingIndexImage> {
    let started = Instant::now();
    let meta = fs_metadata_for_host(&asset.path)?;
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_metadata_ms, started);
    }
    let file_size = meta.len() as i64;
    let mtime_unix = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let path_text = asset.path.to_string_lossy().to_string();
    let Some(fingerprint) = asset.fingerprint else {
        return prepare_index_image_from_metadata(
            &asset.source_root,
            &asset.path,
            fs_path_for_host(&asset.path).as_ref(),
            &path_text,
            file_size,
            mtime_unix,
            true,
            profiler,
        );
    };
    if file_size != asset.file_size || mtime_unix != asset.mtime_unix {
        return prepare_index_image_from_metadata(
            &asset.source_root,
            &asset.path,
            fs_path_for_host(&asset.path).as_ref(),
            &path_text,
            file_size,
            mtime_unix,
            true,
            profiler,
        );
    }
    let started = Instant::now();
    let bytes = fs_read_for_host(&asset.path)?;
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_read_ms, started);
    }
    let started = Instant::now();
    let image = load_embedding_image(&bytes, scaled_jpeg_decode_target, profiler.as_deref_mut())?;
    if let Some(profiler) = profiler {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_keep_image_decode_ms, started);
        profiler.report.prepare_reused_fingerprints += 1;
    }
    Ok(PendingIndexImage {
        source_root: asset.source_root,
        path: asset.path,
        path_text,
        file_size,
        mtime_unix,
        fingerprint,
        image: Some(image),
        reuse_existing_asset: true,
    })
}

fn load_embedding_image(
    bytes: &[u8],
    scaled_jpeg_decode_target: Option<u32>,
    mut profiler: Option<&mut EmbeddingProfiler>,
) -> anyhow::Result<DynamicImage> {
    #[cfg(not(windows))]
    let _ = &mut profiler;
    #[cfg(windows)]
    if let Some(target) = scaled_jpeg_decode_target.filter(|_| is_jpeg_bytes(bytes)) {
        if !jpeg_orientation_requires_dynamic_path(bytes) {
            let started = Instant::now();
            match turbojpeg_scaled_rgb_decode(bytes, target) {
                Ok(decoded) => {
                    if let Some(profiler) = profiler.as_deref_mut() {
                        EmbeddingProfiler::add_ms(
                            &mut profiler.report.prepare_scaled_jpeg_decode_ms,
                            started,
                        );
                        profiler.report.prepare_scaled_jpeg_decode_files += 1;
                        profiler.report.prepare_scaled_jpeg_target_edge = target;
                        profiler.report.prepare_scaled_jpeg_source_pixels +=
                            u64::from(decoded.source_width) * u64::from(decoded.source_height);
                        profiler.report.prepare_scaled_jpeg_output_pixels +=
                            u64::from(decoded.image.width()) * u64::from(decoded.image.height());
                    }
                    return Ok(DynamicImage::ImageRgb8(decoded.image));
                }
                Err(_) => {
                    if let Some(profiler) = profiler.as_deref_mut() {
                        EmbeddingProfiler::add_ms(
                            &mut profiler.report.prepare_scaled_jpeg_decode_ms,
                            started,
                        );
                        profiler.report.prepare_scaled_jpeg_decode_fallbacks += 1;
                    }
                }
            }
        }
    }
    let _ = scaled_jpeg_decode_target;
    Ok(load_dynamic_image_with_orientation(bytes)?.0)
}

fn prepare_index_image(
    con: &Connection,
    source_root: &Path,
    path: &Path,
    force: bool,
    required_embeddings: &[(String, String)],
    keep_image: bool,
    mut profiler: Option<&mut EmbeddingProfiler>,
) -> anyhow::Result<Option<PendingIndexImage>> {
    let started = Instant::now();
    let meta = fs_metadata_for_host(path)?;
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_metadata_ms, started);
    }
    let file_size = meta.len() as i64;
    let mtime_unix = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let path_text = path.to_string_lossy().to_string();
    if !force {
        let started = Instant::now();
        let unchanged =
            existing_unchanged(con, &path_text, file_size, mtime_unix, required_embeddings)?;
        if let Some(profiler) = profiler.as_deref_mut() {
            EmbeddingProfiler::add_ms(&mut profiler.report.prepare_existing_ms, started);
        }
        if unchanged {
            return Ok(None);
        }
    }
    prepare_index_image_from_metadata(
        source_root,
        path,
        fs_path_for_host(path).as_ref(),
        &path_text,
        file_size,
        mtime_unix,
        keep_image,
        profiler,
    )
    .map(Some)
}

#[allow(clippy::too_many_arguments)]
fn prepare_index_image_from_metadata(
    source_root: &Path,
    path: &Path,
    read_path: &Path,
    path_text: &str,
    file_size: i64,
    mtime_unix: i64,
    keep_image: bool,
    profiler: Option<&mut EmbeddingProfiler>,
) -> anyhow::Result<PendingIndexImage> {
    let (fingerprint, image) =
        fingerprint_image_for_index_profiled_at(path, read_path, keep_image, profiler)?;
    Ok(PendingIndexImage {
        source_root: source_root.to_path_buf(),
        path: path.to_path_buf(),
        path_text: path_text.to_string(),
        file_size,
        mtime_unix,
        fingerprint,
        image,
        reuse_existing_asset: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn flush_index_batch_lazy(
    con: &mut Connection,
    options: &BuildOptions,
    want_clip: bool,
    want_sscd: bool,
    clip_vision: &mut Option<ClipVisionRuntime>,
    sscd: &mut Option<SscdRuntime>,
    pending: &mut Vec<PendingIndexImage>,
    preprocessed: Option<PreprocessedEmbeddingInputs>,
    writer: Option<&EmbeddingWriter>,
    profiler: &mut EmbeddingProfiler,
) -> anyhow::Result<FlushStats> {
    if pending.is_empty() {
        return Ok(FlushStats::default());
    }
    ensure_embedding_runtimes(options, want_clip, want_sscd, clip_vision, sscd, profiler)?;
    flush_index_batch(
        con,
        clip_vision.as_ref(),
        sscd.as_ref(),
        pending,
        preprocessed,
        writer,
        profiler,
    )
}

fn ensure_embedding_runtimes(
    options: &BuildOptions,
    want_clip: bool,
    want_sscd: bool,
    clip_vision: &mut Option<ClipVisionRuntime>,
    sscd: &mut Option<SscdRuntime>,
    profiler: &mut EmbeddingProfiler,
) -> anyhow::Result<()> {
    if want_clip && clip_vision.is_none() {
        let started = Instant::now();
        *clip_vision = load_clip_vision_runtime(options)?;
        EmbeddingProfiler::add_ms(&mut profiler.report.load_clip_ms, started);
        if clip_vision.is_none() {
            anyhow::bail!("CLIP embedding was required but no runtime could be loaded");
        }
    }
    if want_sscd && sscd.is_none() {
        let started = Instant::now();
        *sscd = load_sscd_runtime(options)?;
        EmbeddingProfiler::add_ms(&mut profiler.report.load_sscd_ms, started);
        if sscd.is_none() {
            anyhow::bail!("SSCD embedding was required but no runtime could be loaded");
        }
    }
    Ok(())
}

fn flush_index_batch(
    con: &mut Connection,
    clip_vision: Option<&ClipVisionRuntime>,
    sscd: Option<&SscdRuntime>,
    pending: &mut Vec<PendingIndexImage>,
    preprocessed: Option<PreprocessedEmbeddingInputs>,
    writer: Option<&EmbeddingWriter>,
    profiler: &mut EmbeddingProfiler,
) -> anyhow::Result<FlushStats> {
    if pending.is_empty() {
        return Ok(FlushStats::default());
    }
    let items = std::mem::take(pending);
    if clip_vision.is_some() || sscd.is_some() {
        flush_index_batch_with_embeddings(
            con,
            clip_vision,
            sscd,
            items,
            preprocessed,
            writer,
            profiler,
        )
    } else {
        let started = Instant::now();
        let tx = con.transaction()?;
        let indexed_at = current_timestamp();
        for item in &items {
            let asset_id = write_asset_row(&tx, item, &indexed_at)?;
            replace_tile_hashes(&tx, asset_id, &item.fingerprint.tile_hashes)?;
        }
        tx.commit()?;
        EmbeddingProfiler::add_ms(&mut profiler.report.sqlite_ms, started);
        Ok(FlushStats {
            indexed: items.len(),
            errors: 0,
        })
    }
}

fn flush_index_batch_unprofiled(
    con: &mut Connection,
    clip_vision: Option<&ClipVisionRuntime>,
    sscd: Option<&SscdRuntime>,
    pending: &mut Vec<PendingIndexImage>,
) -> anyhow::Result<FlushStats> {
    let mut profiler = EmbeddingProfiler::default();
    flush_index_batch(con, clip_vision, sscd, pending, None, None, &mut profiler)
}

fn flush_index_batch_with_embeddings(
    con: &mut Connection,
    clip_vision: Option<&ClipVisionRuntime>,
    sscd: Option<&SscdRuntime>,
    mut items: Vec<PendingIndexImage>,
    preprocessed: Option<PreprocessedEmbeddingInputs>,
    writer: Option<&EmbeddingWriter>,
    profiler: &mut EmbeddingProfiler,
) -> anyhow::Result<FlushStats> {
    profiler.report.batches += 1;
    profiler.report.images += items.len();
    let images = items
        .iter_mut()
        .map(|item| {
            item.image
                .take()
                .ok_or_else(|| anyhow::anyhow!("missing decoded image for {}", item.path.display()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let (clip_vectors, sscd_vectors) =
        embed_batch_vectors(clip_vision, sscd, &images, preprocessed, profiler);
    match (clip_vectors, sscd_vectors) {
        (clip_vectors, sscd_vectors)
            if batch_vectors_ok(clip_vectors.as_ref(), items.len())
                && batch_vectors_ok(sscd_vectors.as_ref(), items.len()) =>
        {
            let clip_vectors = clip_vectors.transpose()?;
            let sscd_vectors = sscd_vectors.transpose()?;
            let indexed = items.len();
            let batch = EmbeddingWriteBatch {
                items,
                clip_model_key: clip_vision.map(|runtime| runtime.model_key.clone()),
                clip_vectors,
                sscd_model_key: sscd.map(|runtime| runtime.model_key.clone()),
                sscd_vectors,
                indexed_at: current_timestamp(),
            };
            if let Some(writer) = writer {
                writer.enqueue(batch)?;
            } else {
                let (_, sqlite_ms) = write_embedding_batch(con, batch)?;
                profiler.report.sqlite_ms += sqlite_ms;
            }
            Ok(FlushStats { indexed, errors: 0 })
        }
        (clip_vectors, sscd_vectors) => {
            let err = batch_embedding_error(clip_vectors, sscd_vectors, items.len());
            flush_index_batch_single_fallback(
                con,
                clip_vision,
                sscd,
                &items,
                &images,
                err,
                profiler,
            )
        }
    }
}

type EmbeddingBatchResults = (
    Option<anyhow::Result<Vec<Vec<f32>>>>,
    Option<anyhow::Result<Vec<Vec<f32>>>>,
);

fn embed_batch_vectors(
    clip_vision: Option<&ClipVisionRuntime>,
    sscd: Option<&SscdRuntime>,
    images: &[DynamicImage],
    mut preprocessed: Option<PreprocessedEmbeddingInputs>,
    profiler: &mut EmbeddingProfiler,
) -> EmbeddingBatchResults {
    match (clip_vision, sscd) {
        (Some(clip_runtime), Some(sscd_runtime)) => {
            embed_clip_and_sscd_vectors(clip_runtime, sscd_runtime, images, preprocessed, profiler)
        }
        (clip_vision, sscd) => {
            let clip_vectors = match clip_vision {
                Some(runtime) => {
                    let result = embed_clip_vectors_optional(runtime, images, &mut preprocessed);
                    record_clip_batch_profile(result.as_ref().ok(), profiler);
                    Some(result.map(|profile| profile.vectors))
                }
                None => None,
            };
            let sscd_vectors = sscd.map(|runtime| {
                embed_sscd_vectors_optional(runtime, images, &mut preprocessed, profiler)
            });
            (clip_vectors, sscd_vectors)
        }
    }
}

#[cfg(feature = "image-index-clip")]
fn preprocessed_clip_input(
    preprocessed: &mut Option<PreprocessedEmbeddingInputs>,
) -> Option<anyhow::Result<open_clip_inference::vision::VisionPreprocessedBatch>> {
    preprocessed.as_mut().and_then(|inputs| inputs.clip.take())
}

#[cfg(feature = "image-index-sscd")]
fn preprocessed_sscd_input(
    preprocessed: &mut Option<PreprocessedEmbeddingInputs>,
) -> Option<anyhow::Result<(SscdPreprocessedBatch, SscdPreprocessProfile)>> {
    preprocessed.as_mut().and_then(|inputs| inputs.sscd.take())
}

#[cfg(feature = "image-index-clip")]
fn embed_clip_vectors_optional(
    runtime: &ClipVisionRuntime,
    images: &[DynamicImage],
    preprocessed: &mut Option<PreprocessedEmbeddingInputs>,
) -> anyhow::Result<ClipBatchVectors> {
    preprocessed_clip_input(preprocessed)
        .map(|input| input.and_then(|input| runtime.embed_preprocessed_images_profiled(input)))
        .unwrap_or_else(|| runtime.embed_images_profiled(images))
}

#[cfg(not(feature = "image-index-clip"))]
fn embed_clip_vectors_optional(
    runtime: &ClipVisionRuntime,
    images: &[DynamicImage],
    _preprocessed: &mut Option<PreprocessedEmbeddingInputs>,
) -> anyhow::Result<ClipBatchVectors> {
    runtime.embed_images_profiled(images)
}

#[cfg(feature = "image-index-sscd")]
fn embed_sscd_vectors_optional(
    runtime: &SscdRuntime,
    images: &[DynamicImage],
    preprocessed: &mut Option<PreprocessedEmbeddingInputs>,
    profiler: &mut EmbeddingProfiler,
) -> anyhow::Result<Vec<Vec<f32>>> {
    preprocessed_sscd_input(preprocessed)
        .map(|input| match input {
            Ok((input, preprocess_profile)) => runtime.embed_preprocessed_images_profiled(
                input,
                images.len(),
                preprocess_profile,
                profiler,
            ),
            Err(err) => Err(err),
        })
        .unwrap_or_else(|| runtime.embed_images_profiled(images, profiler))
}

#[cfg(not(feature = "image-index-sscd"))]
fn embed_sscd_vectors_optional(
    runtime: &SscdRuntime,
    images: &[DynamicImage],
    _preprocessed: &mut Option<PreprocessedEmbeddingInputs>,
    profiler: &mut EmbeddingProfiler,
) -> anyhow::Result<Vec<Vec<f32>>> {
    runtime.embed_images_profiled(images, profiler)
}

#[cfg(feature = "image-index-sscd")]
fn embed_clip_and_sscd_vectors(
    clip_runtime: &ClipVisionRuntime,
    sscd_runtime: &SscdRuntime,
    images: &[DynamicImage],
    preprocessed: Option<PreprocessedEmbeddingInputs>,
    profiler: &mut EmbeddingProfiler,
) -> EmbeddingBatchResults {
    if let Some(preprocessed) = preprocessed {
        let mut preprocessed = Some(preprocessed);
        let serial_started = Instant::now();
        let clip_vectors = embed_clip_vectors_optional(clip_runtime, images, &mut preprocessed);
        record_clip_batch_profile(clip_vectors.as_ref().ok(), profiler);
        let clip_vectors = clip_vectors.map(|profile| profile.vectors);
        let sscd_vectors =
            embed_sscd_vectors_optional(sscd_runtime, images, &mut preprocessed, profiler);
        profiler.report.clip_sscd_serial_batches += 1;
        EmbeddingProfiler::add_ms(&mut profiler.report.clip_sscd_serial_ms, serial_started);
        return (Some(clip_vectors), Some(sscd_vectors));
    }
    #[cfg(feature = "image-index-clip")]
    if clip_runtime.concurrent_gpu_runs {
        return embed_clip_and_sscd_vectors_concurrent(
            clip_runtime,
            sscd_runtime,
            images,
            profiler,
        );
    }
    let serial_started = Instant::now();
    let (clip_vectors, sscd_vectors) = std::thread::scope(|scope| {
        let tensor_batch_size = sscd_runtime.tensor_batch_size_for(images.len());
        let sscd_input = scope.spawn(|| {
            sscd_preprocess_batch_timed(images, sscd_runtime.input_tensor_type, tensor_batch_size?)
        });
        let clip_vectors = clip_runtime.embed_images_profiled(images);
        record_clip_batch_profile(clip_vectors.as_ref().ok(), profiler);
        let wait_started = Instant::now();
        let sscd_input = sscd_input.join();
        EmbeddingProfiler::add_ms(
            &mut profiler.report.sscd_preprocess_wait_after_clip_ms,
            wait_started,
        );
        let sscd_vectors = match sscd_input {
            Ok(Ok((input, preprocess_profile))) => sscd_runtime.embed_preprocessed_images_profiled(
                input,
                images.len(),
                preprocess_profile,
                profiler,
            ),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(anyhow::anyhow!("SSCD preprocess worker panicked")),
        };
        (clip_vectors, sscd_vectors)
    });
    profiler.report.clip_sscd_serial_batches += 1;
    EmbeddingProfiler::add_ms(&mut profiler.report.clip_sscd_serial_ms, serial_started);
    (
        Some(clip_vectors.map(|profile| profile.vectors)),
        Some(sscd_vectors),
    )
}

#[cfg(all(feature = "image-index-sscd", feature = "image-index-clip"))]
fn embed_clip_and_sscd_vectors_concurrent(
    clip_runtime: &ClipVisionRuntime,
    sscd_runtime: &SscdRuntime,
    images: &[DynamicImage],
    profiler: &mut EmbeddingProfiler,
) -> EmbeddingBatchResults {
    let started = Instant::now();
    let (clip_input, sscd_input) = std::thread::scope(|scope| {
        let clip_input = scope.spawn(|| clip_runtime.preprocess_images_profiled(images));
        let tensor_batch_size = sscd_runtime.tensor_batch_size_for(images.len());
        let sscd_input = scope.spawn(|| {
            sscd_preprocess_batch_timed(images, sscd_runtime.input_tensor_type, tensor_batch_size?)
        });
        let clip_input = match clip_input.join() {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("CLIP preprocess worker panicked")),
        };
        let sscd_input = match sscd_input.join() {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("SSCD preprocess worker panicked")),
        };
        (clip_input, sscd_input)
    });
    let (clip_vectors, sscd_vectors) = std::thread::scope(|scope| {
        let clip_vectors = scope.spawn(|| {
            clip_input.and_then(|input| clip_runtime.embed_preprocessed_images_profiled(input))
        });
        let sscd_vectors = match sscd_input {
            Ok((input, preprocess_profile)) => sscd_runtime.embed_preprocessed_images_profiled(
                input,
                images.len(),
                preprocess_profile,
                profiler,
            ),
            Err(err) => Err(err),
        };
        let clip_vectors = match clip_vectors.join() {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("CLIP inference worker panicked")),
        };
        (clip_vectors, sscd_vectors)
    });
    record_clip_batch_profile(clip_vectors.as_ref().ok(), profiler);
    profiler.report.concurrent_gpu_batches += 1;
    EmbeddingProfiler::add_ms(&mut profiler.report.concurrent_gpu_ms, started);
    (
        Some(clip_vectors.map(|profile| profile.vectors)),
        Some(sscd_vectors),
    )
}

#[cfg(not(feature = "image-index-sscd"))]
fn embed_clip_and_sscd_vectors(
    clip_runtime: &ClipVisionRuntime,
    sscd_runtime: &SscdRuntime,
    images: &[DynamicImage],
    preprocessed: Option<PreprocessedEmbeddingInputs>,
    profiler: &mut EmbeddingProfiler,
) -> EmbeddingBatchResults {
    let mut preprocessed = preprocessed;
    let clip_vectors = embed_clip_vectors_optional(clip_runtime, images, &mut preprocessed);
    record_clip_batch_profile(clip_vectors.as_ref().ok(), profiler);
    (
        Some(clip_vectors.map(|profile| profile.vectors)),
        Some(embed_sscd_vectors_optional(
            sscd_runtime,
            images,
            &mut preprocessed,
            profiler,
        )),
    )
}

#[cfg(all(feature = "image-index-sscd", feature = "image-index-clip"))]
fn concurrent_gpu_runs_enabled_for_provider(execution_provider: &str) -> bool {
    env::var("QQ_ANALYZER_CONCURRENT_GPU_RUNS")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or_else(|| {
            matches!(
                execution_provider.trim().to_ascii_lowercase().as_str(),
                "cuda" | "tensorrt" | "trt"
            )
        })
}

fn batch_vectors_ok(result: Option<&anyhow::Result<Vec<Vec<f32>>>>, expected: usize) -> bool {
    match result {
        Some(Ok(vectors)) => vectors.len() == expected,
        Some(Err(_)) => false,
        None => true,
    }
}

fn batch_embedding_error(
    clip_vectors: Option<anyhow::Result<Vec<Vec<f32>>>>,
    sscd_vectors: Option<anyhow::Result<Vec<Vec<f32>>>>,
    expected: usize,
) -> anyhow::Error {
    let mut parts = Vec::new();
    if let Some(result) = clip_vectors {
        match result {
            Ok(vectors) => parts.push(format!(
                "CLIP batch returned {} vectors for {expected} images",
                vectors.len()
            )),
            Err(err) => parts.push(format!("CLIP batch failed: {err}")),
        }
    }
    if let Some(result) = sscd_vectors {
        match result {
            Ok(vectors) => parts.push(format!(
                "SSCD batch returned {} vectors for {expected} images",
                vectors.len()
            )),
            Err(err) => parts.push(format!("SSCD batch failed: {err}")),
        }
    }
    anyhow::anyhow!(parts.join("; "))
}

fn flush_index_batch_single_fallback(
    con: &mut Connection,
    clip_vision: Option<&ClipVisionRuntime>,
    sscd: Option<&SscdRuntime>,
    items: &[PendingIndexImage],
    images: &[DynamicImage],
    batch_error: anyhow::Error,
    profiler: &mut EmbeddingProfiler,
) -> anyhow::Result<FlushStats> {
    let fallback_started = Instant::now();
    profiler.report.fallback_batches += 1;
    profiler.report.fallback_images += items.len();
    let mut stats = FlushStats::default();
    let batch_error = batch_error.to_string();
    let indexed_at = current_timestamp();
    for (item, image) in items.iter().zip(images.iter()) {
        match embed_single_item(clip_vision, sscd, image) {
            Ok((clip_vector, sscd_vector)) => {
                let started = Instant::now();
                let tx = con.transaction()?;
                write_index_item(
                    &tx,
                    item,
                    clip_vision
                        .map(|runtime| runtime.model_key.as_str())
                        .zip(clip_vector.as_deref()),
                    sscd.map(|runtime| runtime.model_key.as_str())
                        .zip(sscd_vector.as_deref()),
                    &indexed_at,
                    true,
                    true,
                )?;
                tx.commit()?;
                EmbeddingProfiler::add_ms(&mut profiler.report.sqlite_ms, started);
                stats.indexed += 1;
            }
            Err(err) => {
                stats.errors += 1;
                let started = Instant::now();
                record_item_error(
                    con,
                    item,
                    &format!(
                        "embedding batch failed ({batch_error}); single-image fallback failed: {err}"
                    ),
                )?;
                EmbeddingProfiler::add_ms(&mut profiler.report.record_error_ms, started);
            }
        }
    }
    EmbeddingProfiler::add_ms(&mut profiler.report.fallback_ms, fallback_started);
    Ok(stats)
}

type SingleEmbeddingVectors = (Option<Vec<f32>>, Option<Vec<f32>>);

fn embed_single_item(
    clip_vision: Option<&ClipVisionRuntime>,
    sscd: Option<&SscdRuntime>,
    image: &DynamicImage,
) -> anyhow::Result<SingleEmbeddingVectors> {
    let clip_vector = clip_vision
        .map(|runtime| runtime.embed_image(image))
        .transpose()
        .map_err(|err| anyhow::anyhow!("CLIP: {err}"))?;
    let sscd_vector = sscd
        .map(|runtime| runtime.embed_image(image))
        .transpose()
        .map_err(|err| anyhow::anyhow!("SSCD: {err}"))?;
    Ok((clip_vector, sscd_vector))
}

fn write_embedding_batch(
    con: &mut Connection,
    batch: EmbeddingWriteBatch,
) -> anyhow::Result<(usize, f64)> {
    let started = Instant::now();
    let clip_values = batch
        .clip_vectors
        .as_deref()
        .map(prepare_embedding_values)
        .transpose()?;
    let sscd_values = batch
        .sscd_vectors
        .as_deref()
        .map(prepare_embedding_values)
        .transpose()?;
    let tx = con.transaction()?;
    let clip_cleanup_legacy = batch
        .clip_model_key
        .as_deref()
        .map(|model| embedding_legacy_rows_exist(&tx, "clip", model))
        .transpose()?
        .unwrap_or(false);
    let sscd_cleanup_legacy = batch
        .sscd_model_key
        .as_deref()
        .map(|model| embedding_legacy_rows_exist(&tx, "sscd", model))
        .transpose()?
        .unwrap_or(false);
    for (idx, item) in batch.items.iter().enumerate() {
        let clip = batch
            .clip_model_key
            .as_deref()
            .zip(clip_values.as_ref().map(|values| &values[idx]));
        let sscd = batch
            .sscd_model_key
            .as_deref()
            .zip(sscd_values.as_ref().map(|values| &values[idx]));
        write_index_item_prepared(
            &tx,
            item,
            clip,
            sscd,
            &batch.indexed_at,
            clip_cleanup_legacy,
            sscd_cleanup_legacy,
        )?;
    }
    tx.commit()?;
    Ok((batch.items.len(), elapsed_ms_f64(started)))
}

struct PreparedEmbeddingValue {
    dim: i64,
    blob: Vec<u8>,
    sketch_hex: String,
    bucket: i64,
}

fn prepare_embedding_value(vector: &[f32]) -> PreparedEmbeddingValue {
    let sketch = vector_sketch64(vector);
    PreparedEmbeddingValue {
        dim: vector.len() as i64,
        blob: encode_f32_vec(vector),
        sketch_hex: format!("{sketch:016x}"),
        bucket: vector_bucket12(sketch) as i64,
    }
}

fn prepare_embedding_values(vectors: &[Vec<f32>]) -> anyhow::Result<Vec<PreparedEmbeddingValue>> {
    if vectors.is_empty() {
        return Ok(Vec::new());
    }
    let workers = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .min(32)
        .min(vectors.len());
    let chunk_size = vectors.len().div_ceil(workers);
    let mut chunks = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (chunk_index, chunk) in vectors.chunks(chunk_size).enumerate() {
            handles.push(scope.spawn(move || {
                (
                    chunk_index,
                    chunk
                        .iter()
                        .map(|vector| prepare_embedding_value(vector))
                        .collect::<Vec<_>>(),
                )
            }));
        }
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("embedding value prepare worker panicked"))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })?;
    chunks.sort_by_key(|(chunk_index, _)| *chunk_index);
    Ok(chunks.into_iter().flat_map(|(_, values)| values).collect())
}

fn write_index_item_prepared(
    con: &Connection,
    item: &PendingIndexImage,
    clip: Option<(&str, &PreparedEmbeddingValue)>,
    sscd: Option<(&str, &PreparedEmbeddingValue)>,
    indexed_at: &str,
    clip_cleanup_legacy: bool,
    sscd_cleanup_legacy: bool,
) -> anyhow::Result<()> {
    if !item.reuse_existing_asset {
        let asset_id = write_asset_row(con, item, indexed_at)?;
        replace_tile_hashes(con, asset_id, &item.fingerprint.tile_hashes)?;
    }
    if let Some((model_key, value)) = clip {
        upsert_prepared_embedding_at_with_legacy_cleanup(
            con,
            &item.path_text,
            "clip",
            model_key,
            value,
            indexed_at,
            clip_cleanup_legacy,
        )?;
    }
    if let Some((model_key, value)) = sscd {
        upsert_prepared_embedding_at_with_legacy_cleanup(
            con,
            &item.path_text,
            "sscd",
            model_key,
            value,
            indexed_at,
            sscd_cleanup_legacy,
        )?;
    }
    Ok(())
}

fn write_index_item(
    con: &Connection,
    item: &PendingIndexImage,
    clip: Option<(&str, &[f32])>,
    sscd: Option<(&str, &[f32])>,
    indexed_at: &str,
    clip_cleanup_legacy: bool,
    sscd_cleanup_legacy: bool,
) -> anyhow::Result<()> {
    if !item.reuse_existing_asset {
        let asset_id = write_asset_row(con, item, indexed_at)?;
        replace_tile_hashes(con, asset_id, &item.fingerprint.tile_hashes)?;
    }
    if let Some((model_key, vector)) = clip {
        upsert_embedding_at_with_legacy_cleanup(
            con,
            &item.path_text,
            "clip",
            model_key,
            vector,
            indexed_at,
            clip_cleanup_legacy,
        )?;
    }
    if let Some((model_key, vector)) = sscd {
        upsert_embedding_at_with_legacy_cleanup(
            con,
            &item.path_text,
            "sscd",
            model_key,
            vector,
            indexed_at,
            sscd_cleanup_legacy,
        )?;
    }
    Ok(())
}

fn write_asset_row(
    con: &Connection,
    item: &PendingIndexImage,
    indexed_at: &str,
) -> anyhow::Result<i64> {
    let mut stmt = con.prepare_cached(
        r#"
        insert into image_assets
            (path, source_root, file_size, mtime_unix, sha256_hex, phash_hex, phash_algo, width, height, blur_score, blur_algo, quality_flags, source_class, detected_format, has_alpha, orientation_applied, fingerprint_version, indexed_at, stale, error)
        values
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, 0, null)
        on conflict(path) do update set
            source_root=excluded.source_root,
            file_size=excluded.file_size,
            mtime_unix=excluded.mtime_unix,
            sha256_hex=excluded.sha256_hex,
            phash_hex=excluded.phash_hex,
            phash_algo=excluded.phash_algo,
            width=excluded.width,
            height=excluded.height,
            blur_score=excluded.blur_score,
            blur_algo=excluded.blur_algo,
            quality_flags=excluded.quality_flags,
            source_class=excluded.source_class,
            detected_format=excluded.detected_format,
            has_alpha=excluded.has_alpha,
            orientation_applied=excluded.orientation_applied,
            fingerprint_version=excluded.fingerprint_version,
            indexed_at=excluded.indexed_at,
            stale=0,
            error=null
        "#,
    )?;
    stmt.execute(params![
        item.path_text,
        item.source_root.to_string_lossy().to_string(),
        item.file_size,
        item.mtime_unix,
        item.fingerprint.sha256,
        item.fingerprint.phash,
        item.fingerprint.phash_algo,
        item.fingerprint.width as i64,
        item.fingerprint.height as i64,
        item.fingerprint.blur_score,
        item.fingerprint.blur_algo,
        item.fingerprint.quality_flags,
        item.fingerprint.source_class,
        item.fingerprint.detected_format,
        item.fingerprint.has_alpha as i64,
        item.fingerprint.orientation_applied as i64,
        item.fingerprint.fingerprint_version,
        indexed_at,
    ])?;
    Ok(con.query_row(
        "select id from image_assets where path=?1",
        params![item.path_text],
        |row| row.get(0),
    )?)
}

fn replace_tile_hashes(
    con: &Connection,
    asset_id: i64,
    tile_hashes: &[TileHash],
) -> anyhow::Result<()> {
    con.execute(
        "delete from image_asset_tile_hashes where asset_id=?1",
        params![asset_id],
    )?;
    let mut stmt = con.prepare_cached(
        r#"
        insert into image_asset_tile_hashes
            (asset_id, tile_algo, tile_set, tile_index, x0, y0, x1, y1, hash_hex)
        values
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        "#,
    )?;
    for tile in tile_hashes {
        stmt.execute(params![
            asset_id,
            tile.tile_algo,
            tile.tile_set,
            tile.tile_index,
            tile.x0,
            tile.y0,
            tile.x1,
            tile.y1,
            tile.hash_hex,
        ])?;
    }
    Ok(())
}

fn delete_derived_features_for_path(con: &Connection, path: &str) -> anyhow::Result<()> {
    con.execute(
        "delete from image_asset_tile_hashes where asset_id in (select id from image_assets where path=?1)",
        params![path],
    )?;
    con.execute(
        "delete from image_asset_local_features where asset_id in (select id from image_assets where path=?1)",
        params![path],
    )?;
    Ok(())
}

fn existing_unchanged(
    con: &Connection,
    path: &str,
    file_size: i64,
    mtime_unix: i64,
    required_embeddings: &[(String, String)],
) -> anyhow::Result<bool> {
    let mut stmt = con.prepare_cached(
        "select id, file_size, mtime_unix, phash_hex, error, source_class, phash_algo, blur_algo from image_assets where path=?1 and stale=0",
    )?;
    let row = stmt
        .query_row([path], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })
        .optional()?;
    let Some((asset_id, size, mtime, phash, error, source_class, phash_algo, blur_algo)) = row
    else {
        return Ok(false);
    };
    let basic_unchanged = size == file_size
        && mtime == mtime_unix
        && phash.as_deref().unwrap_or("").len() == 16
        && phash_algo == PHASH_ALGO_V2
        && blur_algo == BLUR_ALGO_V2
        && !source_class.is_empty()
        && error.as_deref().unwrap_or("").is_empty()
        && tile_hashes_exist(con, asset_id)?;
    if !basic_unchanged {
        return Ok(false);
    }
    for (kind, model) in required_embeddings {
        if !embedding_exists(con, path, kind, model)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn tile_hashes_exist(con: &Connection, asset_id: i64) -> anyhow::Result<bool> {
    let count: i64 = con.query_row(
        "select count(*) from image_asset_tile_hashes where asset_id=?1 and tile_algo=?2",
        params![asset_id, TILE_ALGO_V1],
        |row| row.get(0),
    )?;
    Ok(count >= tile_regions().len() as i64)
}

fn existing_fast_unchanged(
    con: &Connection,
    path: &str,
    source_root: &str,
    file_size: i64,
    mtime_unix: i64,
    source_class: &str,
) -> anyhow::Result<bool> {
    let mut stmt = con.prepare_cached(
        "select source_root, file_size, mtime_unix, source_class from image_assets where path=?1 and stale=0",
    )?;
    let row = stmt
        .query_row([path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .optional()?;
    Ok(matches!(
        row,
        Some((stored_root, size, mtime, stored_class))
            if stored_root == source_root
                && size == file_size
                && mtime == mtime_unix
                && stored_class == source_class
    ))
}

fn record_path_error(con: &Connection, path: &str, error: &str) -> anyhow::Result<()> {
    let indexed_at = current_timestamp();
    con.execute(
        r#"
        insert into image_assets
            (path, sha256_hex, phash_hex, phash_algo, width, height, blur_score, blur_algo, quality_flags, source_class, detected_format, has_alpha, orientation_applied, fingerprint_version, error, indexed_at, stale)
        values
            (?1, '', null, '', null, null, null, '', '', '', '', 0, 0, '', ?2, ?3, 0)
        on conflict(path) do update set
            sha256_hex='',
            phash_hex=null,
            phash_algo='',
            width=null,
            height=null,
            blur_score=null,
            blur_algo='',
            quality_flags='',
            source_class='',
            detected_format='',
            has_alpha=0,
            orientation_applied=0,
            fingerprint_version='',
            error=excluded.error,
            indexed_at=excluded.indexed_at,
            stale=0
        "#,
        params![path, error, indexed_at],
    )?;
    con.execute("delete from image_embeddings where path=?1", params![path])?;
    delete_derived_features_for_path(con, path)?;
    Ok(())
}

fn record_item_error(
    con: &Connection,
    item: &PendingIndexImage,
    error: &str,
) -> anyhow::Result<()> {
    if item.reuse_existing_asset {
        let indexed_at = current_timestamp();
        con.execute(
            r#"
            update image_assets
            set error=?2, indexed_at=?3, stale=0
            where path=?1
            "#,
            params![item.path_text, error, indexed_at],
        )?;
        con.execute(
            "delete from image_embeddings where path=?1",
            params![item.path_text],
        )?;
        return Ok(());
    }
    let indexed_at = current_timestamp();
    con.execute(
        r#"
        insert into image_assets
            (path, source_root, file_size, mtime_unix, sha256_hex, phash_hex, phash_algo, width, height, blur_score, blur_algo, quality_flags, source_class, detected_format, has_alpha, orientation_applied, fingerprint_version, indexed_at, stale, error)
        values
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, 0, ?19)
        on conflict(path) do update set
            source_root=excluded.source_root,
            file_size=excluded.file_size,
            mtime_unix=excluded.mtime_unix,
            sha256_hex=excluded.sha256_hex,
            phash_hex=excluded.phash_hex,
            phash_algo=excluded.phash_algo,
            width=excluded.width,
            height=excluded.height,
            blur_score=excluded.blur_score,
            blur_algo=excluded.blur_algo,
            quality_flags=excluded.quality_flags,
            source_class=excluded.source_class,
            detected_format=excluded.detected_format,
            has_alpha=excluded.has_alpha,
            orientation_applied=excluded.orientation_applied,
            fingerprint_version=excluded.fingerprint_version,
            indexed_at=excluded.indexed_at,
            stale=0,
            error=excluded.error
        "#,
        params![
            item.path_text,
            item.source_root.to_string_lossy().to_string(),
            item.file_size,
            item.mtime_unix,
            item.fingerprint.sha256,
            item.fingerprint.phash,
            item.fingerprint.phash_algo,
            item.fingerprint.width as i64,
            item.fingerprint.height as i64,
            item.fingerprint.blur_score,
            item.fingerprint.blur_algo,
            item.fingerprint.quality_flags,
            item.fingerprint.source_class,
            item.fingerprint.detected_format,
            item.fingerprint.has_alpha as i64,
            item.fingerprint.orientation_applied as i64,
            item.fingerprint.fingerprint_version,
            indexed_at,
            error,
        ],
    )?;
    con.execute(
        "delete from image_embeddings where path=?1",
        params![item.path_text],
    )?;
    delete_derived_features_for_path(con, &item.path_text)?;
    Ok(())
}

fn reset_seen_paths(con: &Connection) -> anyhow::Result<()> {
    con.execute_batch(
        r#"
        create temp table if not exists image_index_seen_paths(
            path text primary key
        );
        delete from image_index_seen_paths;
        "#,
    )?;
    Ok(())
}

fn remember_seen_path(con: &Connection, path: &str) -> anyhow::Result<bool> {
    let mut stmt =
        con.prepare_cached("insert or ignore into image_index_seen_paths(path) values (?1)")?;
    Ok(stmt.execute([path])? > 0)
}

fn mark_missing_assets_stale(con: &mut Connection) -> anyhow::Result<usize> {
    let tx = con.transaction()?;
    let changed = tx.execute(
        r#"
        update image_assets
        set stale=1
        where stale=0
          and not exists (
              select 1
              from image_index_seen_paths seen
              where seen.path=image_assets.path
          )
        "#,
        [],
    )?;
    tx.commit()?;
    Ok(changed)
}

#[cfg(test)]
fn upsert_embedding(
    con: &Connection,
    path: &str,
    kind: &str,
    model: &str,
    vector: &[f32],
) -> anyhow::Result<()> {
    upsert_embedding_at(con, path, kind, model, vector, &current_timestamp())
}

#[cfg(test)]
fn upsert_embedding_at(
    con: &Connection,
    path: &str,
    kind: &str,
    model: &str,
    vector: &[f32],
    updated_at: &str,
) -> anyhow::Result<()> {
    upsert_embedding_at_with_legacy_cleanup(con, path, kind, model, vector, updated_at, true)
}

fn upsert_embedding_at_with_legacy_cleanup(
    con: &Connection,
    path: &str,
    kind: &str,
    model: &str,
    vector: &[f32],
    updated_at: &str,
    cleanup_legacy: bool,
) -> anyhow::Result<()> {
    let value = prepare_embedding_value(vector);
    upsert_prepared_embedding_at_with_legacy_cleanup(
        con,
        path,
        kind,
        model,
        &value,
        updated_at,
        cleanup_legacy,
    )
}

fn upsert_prepared_embedding_at_with_legacy_cleanup(
    con: &Connection,
    path: &str,
    kind: &str,
    model: &str,
    value: &PreparedEmbeddingValue,
    updated_at: &str,
    cleanup_legacy: bool,
) -> anyhow::Result<()> {
    {
        let mut stmt = con.prepare_cached(
            r#"
            insert into image_embeddings (path, kind, model, dim, vec, sketch64_hex, bucket12, updated_at)
            values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            on conflict(path, kind, model) do update set
                dim=excluded.dim,
                vec=excluded.vec,
                sketch64_hex=excluded.sketch64_hex,
                bucket12=excluded.bucket12,
                updated_at=excluded.updated_at
            "#,
        )?;
        stmt.execute(params![
            path,
            kind,
            model,
            value.dim,
            &value.blob,
            &value.sketch_hex,
            value.bucket,
            updated_at,
        ])?;
    }
    if !cleanup_legacy {
        return Ok(());
    }
    let (legacy_start, legacy_end) = legacy_model_key_bounds(model);
    let mut stmt = con.prepare_cached(
        "delete from image_embeddings where path=?1 and kind=?2 and model>=?3 and model<?4",
    )?;
    stmt.execute(params![path, kind, legacy_start, legacy_end])?;
    Ok(())
}

fn embedding_legacy_rows_exist(con: &Connection, kind: &str, model: &str) -> anyhow::Result<bool> {
    let (legacy_start, legacy_end) = legacy_model_key_bounds(model);
    let exists: i64 = con.query_row(
        r#"
        select exists(
            select 1
            from image_embeddings
            where kind=?1
              and model>=?2
              and model<?3
            limit 1
        )
        "#,
        params![kind, legacy_start, legacy_end],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

fn current_timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn embedding_exists(con: &Connection, path: &str, kind: &str, model: &str) -> anyhow::Result<bool> {
    let (legacy_start, legacy_end) = legacy_model_key_bounds(model);
    let mut stmt = con.prepare_cached(
        r#"
        select count(*)
        from image_embeddings
        where path=?1
          and kind=?2
          and (model=?3 or (model>=?4 and model<?5))
          and length(sketch64_hex)=16
          and bucket12 is not null
          and bucket12>=0
        "#,
    )?;
    let count: i64 = stmt.query_row(
        params![path, kind, model, legacy_start, legacy_end],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn backfill_embedding_metadata(con: &mut Connection) -> anyhow::Result<usize> {
    let mut total = 0usize;
    let has_missing: i64 = con.query_row(
        r#"
        select exists(
            select 1
            from image_embeddings
            where sketch64_hex='' or bucket12 is null
            limit 1
        )
        "#,
        [],
        |row| row.get(0),
    )?;
    if has_missing == 0 {
        return Ok(0);
    }
    loop {
        let rows = {
            let mut stmt = con.prepare(
                "select rowid, vec from image_embeddings where sketch64_hex='' or bucket12 is null limit 1000",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        if rows.is_empty() {
            break;
        }
        let tx = con.transaction()?;
        for (rowid, blob) in rows {
            if let Some(sketch) = vector_sketch64_blob(&blob) {
                tx.execute(
                    "update image_embeddings set sketch64_hex=?1, bucket12=?2 where rowid=?3",
                    params![
                        format!("{sketch:016x}"),
                        vector_bucket12(sketch) as i64,
                        rowid
                    ],
                )?;
                total += 1;
            } else {
                tx.execute(
                    "update image_embeddings set sketch64_hex='invalid', bucket12=-1 where rowid=?1",
                    params![rowid],
                )?;
            }
        }
        tx.commit()?;
    }
    Ok(total)
}

struct LegacyEmbeddingRow {
    path: String,
    legacy_model: String,
    dim: i64,
    vec: Vec<u8>,
    sketch64_hex: String,
    bucket12: Option<i64>,
    updated_at: String,
}

fn canonicalize_legacy_model_keys(
    con: &mut Connection,
    required_embeddings: &[(String, String)],
) -> anyhow::Result<usize> {
    let mut total = 0usize;
    for (kind, model) in required_embeddings {
        let (legacy_start, legacy_end) = legacy_model_key_bounds(model);
        loop {
            let rows = {
                let mut stmt = con.prepare(
                    r#"
                    select path, model, dim, vec, sketch64_hex, bucket12, updated_at
                    from image_embeddings
                    where kind=?1
                      and model>=?2
                      and model<?3
                    order by path, updated_at desc, model
                    limit 1000
                    "#,
                )?;
                let mapped = stmt.query_map(params![kind, legacy_start, legacy_end], |row| {
                    Ok(LegacyEmbeddingRow {
                        path: row.get(0)?,
                        legacy_model: row.get(1)?,
                        dim: row.get(2)?,
                        vec: row.get(3)?,
                        sketch64_hex: row.get(4)?,
                        bucket12: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                })?;
                mapped.collect::<Result<Vec<_>, _>>()?
            };
            if rows.is_empty() {
                break;
            }
            let tx = con.transaction()?;
            for row in rows {
                tx.execute(
                    r#"
                    insert into image_embeddings
                        (path, kind, model, dim, vec, sketch64_hex, bucket12, updated_at)
                    values
                        (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                    on conflict(path, kind, model) do update set
                        dim=excluded.dim,
                        vec=excluded.vec,
                        sketch64_hex=excluded.sketch64_hex,
                        bucket12=excluded.bucket12,
                        updated_at=excluded.updated_at
                    where excluded.updated_at > image_embeddings.updated_at
                    "#,
                    params![
                        row.path,
                        kind,
                        model,
                        row.dim,
                        row.vec,
                        row.sketch64_hex,
                        row.bucket12,
                        row.updated_at,
                    ],
                )?;
                total += tx.execute(
                    "delete from image_embeddings where path=?1 and kind=?2 and model=?3",
                    params![row.path, kind, row.legacy_model],
                )?;
            }
            tx.commit()?;
        }
    }
    Ok(total)
}

impl QueryRuntimeCache {
    fn clip_vision_runtime(
        &mut self,
        options: &BuildOptions,
    ) -> anyhow::Result<Option<&ClipVisionRuntime>> {
        let Some(key) = clip_runtime_cache_key(options) else {
            self.clip_vision = None;
            return Ok(None);
        };
        let reload = self
            .clip_vision
            .as_ref()
            .map(|cached| cached.key != key)
            .unwrap_or(true);
        if reload {
            self.clip_vision = load_clip_vision_runtime(options)?
                .map(|runtime| CachedClipVisionRuntime { key, runtime });
        }
        Ok(self.clip_vision.as_ref().map(|cached| &cached.runtime))
    }

    fn clip_text_runtime(
        &mut self,
        options: &BuildOptions,
    ) -> anyhow::Result<Option<&ClipTextRuntime>> {
        let Some(key) = clip_runtime_cache_key(options) else {
            self.clip_text = None;
            return Ok(None);
        };
        let reload = self
            .clip_text
            .as_ref()
            .map(|cached| cached.key != key)
            .unwrap_or(true);
        if reload {
            self.clip_text = load_clip_text_runtime(options)?
                .map(|runtime| CachedClipTextRuntime { key, runtime });
        }
        Ok(self.clip_text.as_ref().map(|cached| &cached.runtime))
    }

    fn sscd_runtime(&mut self, options: &BuildOptions) -> anyhow::Result<Option<&SscdRuntime>> {
        let Some(key) = sscd_runtime_cache_key(options) else {
            self.sscd = None;
            return Ok(None);
        };
        let reload = self
            .sscd
            .as_ref()
            .map(|cached| cached.key != key)
            .unwrap_or(true);
        if reload {
            self.sscd =
                load_sscd_runtime(options)?.map(|runtime| CachedSscdRuntime { key, runtime });
        }
        Ok(self.sscd.as_ref().map(|cached| &cached.runtime))
    }
}

fn clip_runtime_cache_key(options: &BuildOptions) -> Option<RuntimeCacheKey> {
    resolve_clip_model_dir(options).map(|path| RuntimeCacheKey {
        model: options.clip_model.clone(),
        path,
        execution_provider: normalized_execution_provider(&options.execution_provider),
    })
}

fn sscd_runtime_cache_key(options: &BuildOptions) -> Option<RuntimeCacheKey> {
    resolve_sscd_model_path(options).map(|path| RuntimeCacheKey {
        model: options.sscd_model.clone(),
        path,
        execution_provider: normalized_execution_provider(&options.execution_provider),
    })
}

fn normalized_execution_provider(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        "auto".to_string()
    } else {
        value
    }
}

fn append_copy_results(
    con: &Connection,
    results: &mut Vec<QueryResult>,
    runtime: &SscdRuntime,
    query_image: Option<&DynamicImage>,
    image_path: &Path,
    query_strategy: QueryStrategy,
    limit: usize,
) -> anyhow::Result<()> {
    let vector = if let Some(image) = query_image {
        runtime.embed_image(image)?
    } else {
        runtime.embed_image_path(image_path)?
    };
    for item in semantic_matches(
        con,
        "sscd",
        &runtime.model_key,
        &vector,
        query_strategy,
        expanded_query_limit(limit),
    )? {
        push_result(
            results,
            sscd_match_kind(item.1),
            item.1,
            None,
            item.0,
            limit,
        );
    }
    Ok(())
}

fn append_semantic_image_results(
    con: &Connection,
    results: &mut Vec<QueryResult>,
    runtime: &ClipVisionRuntime,
    query_image: Option<&DynamicImage>,
    image_path: &Path,
    query_strategy: QueryStrategy,
    limit: usize,
) -> anyhow::Result<()> {
    let vector = if let Some(image) = query_image {
        runtime.embed_image(image)?
    } else {
        runtime.embed_image_path(image_path)?
    };
    for item in semantic_matches(
        con,
        "clip",
        &runtime.model_key,
        &vector,
        query_strategy,
        expanded_query_limit(limit),
    )? {
        push_result(results, "semantic_clip", item.1, None, item.0, limit);
    }
    Ok(())
}

fn semantic_text_results(
    con: &Connection,
    runtime: &ClipTextRuntime,
    text: &str,
    query_strategy: QueryStrategy,
    limit: usize,
) -> anyhow::Result<Vec<QueryResult>> {
    let limit = normalize_query_limit(limit);
    let vector = runtime.embed_text(text)?;
    Ok(semantic_matches(
        con,
        "clip",
        &runtime.model_key,
        &vector,
        query_strategy,
        expanded_query_limit(limit),
    )?
    .into_iter()
    .take(limit)
    .map(|(item, score)| query_result_from_item("semantic_clip", score, None, item))
    .collect())
}

fn semantic_matches(
    con: &Connection,
    kind: &str,
    model: &str,
    query: &[f32],
    query_strategy: QueryStrategy,
    limit: usize,
) -> anyhow::Result<Vec<(IndexedImage, f64)>> {
    let limit = normalize_query_limit(limit);
    if query_strategy == QueryStrategy::Exact {
        return semantic_full_scan(con, kind, model, query, limit);
    }
    let shortlist = semantic_bucket_shortlist(con, kind, model, query, limit)?;
    // Fast is a latency contract. Sparse sketch buckets used to fall through
    // to a multi-million-row vector scan, which made the default web search
    // appear to hang for minutes. Exact mode remains available when an
    // exhaustive ranking is explicitly requested.
    Ok(sort_semantic_results(shortlist.results))
}

struct SemanticScan {
    results: Vec<(IndexedImage, f64)>,
}

fn semantic_bucket_shortlist(
    con: &Connection,
    kind: &str,
    model: &str,
    query: &[f32],
    limit: usize,
) -> anyhow::Result<SemanticScan> {
    // The web-facing fast strategy is deliberately a single-bucket lookup.
    // Probing every Hamming-1 neighbor multiplies random vector reads by 13 on
    // the largest manifests. Exact mode remains the exhaustive alternative.
    let buckets = vec![vector_bucket12(vector_sketch64(query))];
    let placeholders = (0..buckets.len())
        .map(|idx| format!("?{}", idx + 6))
        .collect::<Vec<_>>()
        .join(",");
    let scan_limit_param = 6 + buckets.len();
    let (legacy_start, legacy_end) = legacy_model_key_bounds(model);
    let sql = format!(
        r#"
        select e.vec, e.path
        from image_embeddings e
        where e.kind=?1
          and (e.model=?2 or (e.model>=?3 and e.model<?4))
          and e.dim=?5
          and e.bucket12 in ({placeholders})
          and e.sketch64_hex!=''
        limit ?{scan_limit_param}
        "#
    );
    let mut params = Vec::with_capacity(5 + buckets.len());
    params.push(rusqlite::types::Value::Text(kind.to_string()));
    params.push(rusqlite::types::Value::Text(model.to_string()));
    params.push(rusqlite::types::Value::Text(legacy_start));
    params.push(rusqlite::types::Value::Text(legacy_end));
    params.push(rusqlite::types::Value::Integer(query.len() as i64));
    for bucket in buckets {
        params.push(rusqlite::types::Value::Integer(bucket as i64));
    }
    params.push(rusqlite::types::Value::Integer(
        FAST_SEMANTIC_VECTOR_SCAN_CAP.max(limit) as i64,
    ));
    let mut stmt = con.prepare_cached(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    hydrate_semantic_scan(con, collect_semantic_rows(&mut rows, query, limit)?)
}

fn semantic_full_scan(
    con: &Connection,
    kind: &str,
    model: &str,
    query: &[f32],
    limit: usize,
) -> anyhow::Result<Vec<(IndexedImage, f64)>> {
    let (legacy_start, legacy_end) = legacy_model_key_bounds(model);
    let mut stmt = con.prepare_cached(
        r#"
        select e.vec, e.path
        from image_embeddings e
        where e.kind=?1
          and (e.model=?2 or (e.model>=?3 and e.model<?4))
          and e.dim=?5
        "#,
    )?;
    let mut rows = stmt.query(params![
        kind,
        model,
        legacy_start,
        legacy_end,
        query.len() as i64
    ])?;
    Ok(sort_semantic_results(
        hydrate_semantic_scan(con, collect_semantic_rows(&mut rows, query, limit)?)?.results,
    ))
}

fn legacy_model_key_bounds(model: &str) -> (String, String) {
    (format!("{model}:"), format!("{model};"))
}

fn collect_semantic_rows(
    rows: &mut rusqlite::Rows<'_>,
    query: &[f32],
    limit: usize,
) -> anyhow::Result<SemanticScan> {
    let mut out: Vec<(IndexedImage, f64)> = Vec::with_capacity(limit);
    while let Some(row) = rows.next()? {
        let Some(score) = dot_f32_row_blob(query, row, 0)? else {
            continue;
        };
        let score = score as f64;
        if !semantic_score_can_enter(&out, score, limit) {
            continue;
        }
        // Metadata hydration is deliberately deferred until after the vector
        // Top-K is known.  On the largest manifest a skewed CLIP bucket can
        // contain tens of thousands of vectors; joining image_assets here
        // caused one random metadata lookup for every candidate.
        let item = IndexedImage {
            path: PathBuf::from(row.get::<_, String>(1)?),
            sha256: String::new(),
            phash: None,
            phash_algo: None,
            width: None,
            height: None,
            blur_score: None,
            blur_algo: None,
            quality_flags: String::new(),
            source_class: String::new(),
            tile_algo: None,
            matched_tile_count: None,
        };
        push_top_semantic(&mut out, (item, score), limit);
    }
    Ok(SemanticScan { results: out })
}

fn hydrate_semantic_scan(con: &Connection, scan: SemanticScan) -> anyhow::Result<SemanticScan> {
    let mut stmt = con.prepare_cached(
        r#"select path, sha256_hex, phash_hex, phash_algo, width, height,
                  blur_score, blur_algo, quality_flags, source_class
           from image_assets
           where path=?1 and stale=0"#,
    )?;
    let mut results = Vec::with_capacity(scan.results.len());
    for (item, score) in scan.results {
        if let Some(hydrated) = stmt
            .query_row([item.path.to_string_lossy().as_ref()], image_from_row)
            .optional()?
        {
            results.push((hydrated, score));
        }
    }
    Ok(SemanticScan { results })
}

fn sort_semantic_results(mut results: Vec<(IndexedImage, f64)>) -> Vec<(IndexedImage, f64)> {
    results.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.path.cmp(&b.0.path))
    });
    results
}

fn semantic_score_can_enter(out: &[(IndexedImage, f64)], score: f64, limit: usize) -> bool {
    if out.len() < limit {
        return true;
    }
    let Some(worst) = out.last() else {
        return true;
    };
    !matches!(score.partial_cmp(&worst.1), Some(std::cmp::Ordering::Less))
}

fn push_top_semantic(
    out: &mut Vec<(IndexedImage, f64)>,
    candidate: (IndexedImage, f64),
    limit: usize,
) {
    if limit == 0 {
        return;
    }
    if out.len() >= limit {
        if let Some(worst) = out.last() {
            if !semantic_candidate_better(&candidate, worst) {
                return;
            }
        }
    }
    if let Some(existing_idx) = out
        .iter()
        .position(|existing| existing.0.path == candidate.0.path)
    {
        if semantic_candidate_better(&candidate, &out[existing_idx]) {
            out.remove(existing_idx);
            insert_top_semantic_sorted(out, candidate);
        }
        return;
    }
    if out.len() >= limit {
        out.pop();
    }
    insert_top_semantic_sorted(out, candidate);
}

fn insert_top_semantic_sorted(out: &mut Vec<(IndexedImage, f64)>, candidate: (IndexedImage, f64)) {
    let pos = out
        .iter()
        .position(|existing| semantic_candidate_better(&candidate, existing))
        .unwrap_or(out.len());
    out.insert(pos, candidate);
}

fn semantic_candidate_better(a: &(IndexedImage, f64), b: &(IndexedImage, f64)) -> bool {
    match a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => a.0.path < b.0.path,
    }
}

fn encode_f32_vec(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn dot_f32_blob(query: &[f32], bytes: &[u8]) -> Option<f32> {
    if bytes.len() != query.len() * 4 {
        return None;
    }
    let mut score = 0.0f32;
    for (value, chunk) in query.iter().zip(bytes.chunks_exact(4)) {
        let other = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        score += value * other;
    }
    Some(score)
}

fn dot_f32_row_blob(
    query: &[f32],
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Option<f32>> {
    match row.get_ref(column)? {
        rusqlite::types::ValueRef::Blob(bytes) => Ok(dot_f32_blob(query, bytes)),
        _ => Ok(None),
    }
}

fn vector_sketch64(vector: &[f32]) -> u64 {
    vector_sketch64_values(vector.len(), vector.iter().copied())
}

fn vector_sketch64_values(dim: usize, values: impl Iterator<Item = f32>) -> u64 {
    let signs = sketch_projection_signs(dim);
    let mut acc = [0.0f32; VECTOR_SKETCH_BITS];
    for (idx, value) in values.enumerate() {
        let row = &signs[idx];
        for (acc_bit, sign) in acc.iter_mut().zip(row.iter()) {
            *acc_bit += value * *sign;
        }
    }
    let mut sketch = 0u64;
    for (bit, value) in acc.iter().enumerate() {
        if *value >= 0.0 {
            sketch |= 1u64 << bit;
        }
    }
    sketch
}

fn vector_sketch64_blob(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(vector_sketch64_values(
        bytes.len() / 4,
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])),
    ))
}

fn vector_bucket12(sketch: u64) -> u16 {
    (sketch >> (64 - VECTOR_BUCKET_BITS)) as u16
}

#[cfg(test)]
fn vector_probe_buckets(base: u16) -> Vec<u16> {
    let mut buckets = vec![base];
    if VECTOR_PROBE_MAX_HAMMING >= 1 {
        for bit in 0..VECTOR_BUCKET_BITS {
            buckets.push(base ^ (1u16 << bit));
        }
    }
    buckets
}

fn sketch_projection_positive(dim: usize, bit: usize) -> bool {
    let seed = ((dim as u64) << 32) ^ bit as u64 ^ 0x9e37_79b9_7f4a_7c15;
    splitmix64(seed) & 1 == 1
}

type SketchProjectionSigns = Vec<[f32; VECTOR_SKETCH_BITS]>;

fn sketch_projection_signs(dim: usize) -> Arc<SketchProjectionSigns> {
    static CACHE: OnceLock<Mutex<HashMap<usize, Arc<SketchProjectionSigns>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(signs) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&dim)
        .cloned()
    {
        return signs;
    }

    let signs = Arc::new(build_sketch_projection_signs(dim));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .entry(dim)
        .or_insert_with(|| Arc::clone(&signs))
        .clone()
}

fn build_sketch_projection_signs(dim: usize) -> SketchProjectionSigns {
    let mut signs = Vec::with_capacity(dim);
    for idx in 0..dim {
        let mut row = [0.0f32; VECTOR_SKETCH_BITS];
        for (bit, sign) in row.iter_mut().enumerate() {
            *sign = if sketch_projection_positive(idx, bit) {
                1.0
            } else {
                -1.0
            };
        }
        signs.push(row);
    }
    signs
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn fingerprint_image_for_index(
    path: &Path,
    keep_image: bool,
) -> anyhow::Result<(ImageFingerprint, Option<DynamicImage>)> {
    fingerprint_image_for_index_profiled(path, keep_image, None)
}

fn fingerprint_image_for_index_profiled(
    path: &Path,
    keep_image: bool,
    profiler: Option<&mut EmbeddingProfiler>,
) -> anyhow::Result<(ImageFingerprint, Option<DynamicImage>)> {
    fingerprint_image_for_index_profiled_at(
        path,
        fs_path_for_host(path).as_ref(),
        keep_image,
        profiler,
    )
}

fn fingerprint_image_for_index_profiled_at(
    path: &Path,
    read_path: &Path,
    keep_image: bool,
    mut profiler: Option<&mut EmbeddingProfiler>,
) -> anyhow::Result<(ImageFingerprint, Option<DynamicImage>)> {
    let started = Instant::now();
    let bytes = fs::read(read_path)?;
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_read_ms, started);
    }
    let started = Instant::now();
    let sha256 = sha256_bytes(&bytes);
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_sha256_ms, started);
    }
    fingerprint_image_bytes_for_index_profiled(path, &bytes, sha256, keep_image, profiler)
}

fn fingerprint_image_bytes_for_index(
    path: &Path,
    bytes: &[u8],
    sha256: String,
    keep_image: bool,
) -> anyhow::Result<(ImageFingerprint, Option<DynamicImage>)> {
    fingerprint_image_bytes_for_index_profiled(path, bytes, sha256, keep_image, None)
}

fn fingerprint_image_bytes_for_index_profiled(
    path: &Path,
    bytes: &[u8],
    sha256: String,
    keep_image: bool,
    mut profiler: Option<&mut EmbeddingProfiler>,
) -> anyhow::Result<(ImageFingerprint, Option<DynamicImage>)> {
    if is_jpeg_bytes(bytes) {
        let started = Instant::now();
        let requires_dynamic = jpeg_orientation_requires_dynamic_path(bytes);
        if let Some(profiler) = profiler.as_deref_mut() {
            EmbeddingProfiler::add_ms(&mut profiler.report.prepare_jpeg_orientation_ms, started);
        }
        if !requires_dynamic {
            if let Ok(fingerprint) =
                jpeg_luma_fingerprint_profiled(path, bytes, sha256.clone(), profiler.as_deref_mut())
            {
                let image = if keep_image {
                    let started = Instant::now();
                    let image = load_dynamic_image_with_orientation(bytes)?.0;
                    if let Some(profiler) = profiler.as_deref_mut() {
                        EmbeddingProfiler::add_ms(
                            &mut profiler.report.prepare_keep_image_decode_ms,
                            started,
                        );
                    }
                    Some(image)
                } else {
                    None
                };
                return Ok((fingerprint, image));
            }
        }
    }
    let started = Instant::now();
    let (image, orientation_applied) = load_dynamic_image_with_orientation(bytes)?;
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_dynamic_decode_ms, started);
    }
    let fingerprint = fingerprint_from_dynamic_image_profiled(
        path,
        bytes,
        sha256,
        &image,
        orientation_applied,
        profiler,
    );
    Ok((fingerprint, keep_image.then_some(image)))
}

fn fingerprint_from_dynamic_image_profiled(
    path: &Path,
    bytes: &[u8],
    sha256: String,
    image: &DynamicImage,
    orientation_applied: bool,
    mut profiler: Option<&mut EmbeddingProfiler>,
) -> ImageFingerprint {
    let started = Instant::now();
    let luma = normalized_luma_from_dynamic_image(path, bytes, image, orientation_applied);
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_luma_ms, started);
    }
    fingerprint_from_normalized_luma_profiled(path, sha256, luma, profiler)
}

fn jpeg_luma_fingerprint_profiled(
    path: &Path,
    bytes: &[u8],
    sha256: String,
    mut profiler: Option<&mut EmbeddingProfiler>,
) -> anyhow::Result<ImageFingerprint> {
    let started = Instant::now();
    let (width, height, luma) = zune_jpeg_luma_decode(bytes)?;
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_jpeg_luma_ms, started);
    }
    let normalized = NormalizedLumaImage {
        width,
        height,
        pixels: luma,
        detected_format: image_format_label(bytes, path),
        has_alpha: false,
        orientation_applied: false,
    };
    Ok(fingerprint_from_normalized_luma_profiled(
        path, sha256, normalized, profiler,
    ))
}

fn zune_jpeg_luma_decode(bytes: &[u8]) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::Luma);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder
        .decode_headers()
        .map_err(|err| anyhow::anyhow!("decode JPEG headers: {err}"))?;
    if decoder.output_colorspace() != Some(ColorSpace::Luma) {
        anyhow::bail!("JPEG decoder did not accept luma output");
    }
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| anyhow::anyhow!("missing JPEG dimensions"))?;
    let mut luma = vec![
        0u8;
        decoder
            .output_buffer_size()
            .ok_or_else(|| anyhow::anyhow!("missing JPEG output buffer size"))?
    ];
    decoder
        .decode_into(&mut luma)
        .map_err(|err| anyhow::anyhow!("decode JPEG luma: {err}"))?;
    let expected = width
        .checked_mul(height)
        .ok_or_else(|| anyhow::anyhow!("JPEG dimensions overflow"))?;
    if luma.len() < expected {
        anyhow::bail!(
            "JPEG luma output too small: {} bytes for {}x{}",
            luma.len(),
            width,
            height
        );
    }
    Ok((width as u32, height as u32, luma[..expected].to_vec()))
}

fn fingerprint_image_bytes_for_turbojpeg_benchmark(
    path: &Path,
    bytes: &[u8],
    sha256: String,
) -> anyhow::Result<ImageFingerprint> {
    if is_jpeg_bytes(bytes) && !jpeg_orientation_requires_dynamic_path(bytes) {
        ensure_turbojpeg_benchmark_backend()?;
        #[cfg(windows)]
        {
            if let Ok(fingerprint) = turbojpeg_luma_fingerprint(path, bytes, sha256.clone()) {
                return Ok(fingerprint);
            }
        }
    }
    Ok(fingerprint_image_bytes_for_index(path, bytes, sha256, false)?.0)
}

#[cfg(windows)]
fn turbojpeg_luma_fingerprint(
    path: &Path,
    bytes: &[u8],
    sha256: String,
) -> anyhow::Result<ImageFingerprint> {
    let (width, height, luma) = turbojpeg_luma_decode(bytes)?;
    let normalized = NormalizedLumaImage {
        width,
        height,
        pixels: luma,
        detected_format: image_format_label(bytes, path),
        has_alpha: false,
        orientation_applied: false,
    };
    Ok(fingerprint_from_normalized_luma_profiled(
        path, sha256, normalized, None,
    ))
}

#[cfg(windows)]
fn turbojpeg_luma_decode(bytes: &[u8]) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let api = turbojpeg_api().ok_or_else(|| {
        anyhow::anyhow!("QQ_ANALYZER_TURBOJPEG_DLL is not set or turbojpeg.dll failed to load")
    })?;
    unsafe {
        let handle = (api.tj3_init_version)(TJINIT_DECOMPRESS, TURBOJPEG_VERSION_NUMBER);
        if handle.is_null() {
            anyhow::bail!("tj3InitVersion failed");
        }
        let _guard = TurboJpegHandle { api, handle };
        if (api.tj3_decompress_header)(handle, bytes.as_ptr(), bytes.len()) != 0 {
            anyhow::bail!("{}", turbojpeg_error(api, handle));
        }
        let width = (api.tj3_get)(handle, TJPARAM_JPEGWIDTH);
        let height = (api.tj3_get)(handle, TJPARAM_JPEGHEIGHT);
        if width <= 0 || height <= 0 {
            anyhow::bail!("invalid TurboJPEG dimensions: {width}x{height}");
        }
        let len = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow::anyhow!("TurboJPEG dimensions overflow"))?;
        let mut luma = vec![0u8; len];
        if (api.tj3_decompress8)(
            handle,
            bytes.as_ptr(),
            bytes.len(),
            luma.as_mut_ptr(),
            width,
            TJPF_GRAY,
        ) != 0
        {
            anyhow::bail!("{}", turbojpeg_error(api, handle));
        }
        Ok((width as u32, height as u32, luma))
    }
}

#[cfg(windows)]
struct TurboJpegScaledImage {
    image: RgbImage,
    source_width: u32,
    source_height: u32,
}

#[cfg(windows)]
fn turbojpeg_scaled_rgb_decode(bytes: &[u8], target: u32) -> anyhow::Result<TurboJpegScaledImage> {
    let api = turbojpeg_api().ok_or_else(|| {
        anyhow::anyhow!("QQ_ANALYZER_TURBOJPEG_DLL is not set or turbojpeg.dll failed to load")
    })?;
    unsafe {
        let handle = (api.tj3_init_version)(TJINIT_DECOMPRESS, TURBOJPEG_VERSION_NUMBER);
        if handle.is_null() {
            anyhow::bail!("tj3InitVersion failed");
        }
        let _guard = TurboJpegHandle { api, handle };
        if (api.tj3_decompress_header)(handle, bytes.as_ptr(), bytes.len()) != 0 {
            anyhow::bail!("{}", turbojpeg_error(api, handle));
        }
        if (api.tj3_get)(handle, TJPARAM_LOSSLESS) != 0 {
            anyhow::bail!("TurboJPEG IDCT scaling is unavailable for lossless JPEG");
        }
        let width = (api.tj3_get)(handle, TJPARAM_JPEGWIDTH);
        let height = (api.tj3_get)(handle, TJPARAM_JPEGHEIGHT);
        if width <= 0 || height <= 0 {
            anyhow::bail!("invalid TurboJPEG dimensions: {width}x{height}");
        }
        let mut factor_count = 0;
        let factor_ptr = (api.tj3_get_scaling_factors)(&mut factor_count);
        if factor_ptr.is_null() || !(1..=64).contains(&factor_count) {
            anyhow::bail!("invalid TurboJPEG scaling factor count: {factor_count}");
        }
        let factors = std::slice::from_raw_parts(factor_ptr, factor_count as usize);
        let factor = select_jpeg_scaling_factor(width as u32, height as u32, target, factors);
        if (api.tj3_set_scaling_factor)(handle, factor) != 0 {
            anyhow::bail!("{}", turbojpeg_error(api, handle));
        }
        let scaled_width = jpeg_scaled_dimension(width as u32, factor)
            .ok_or_else(|| anyhow::anyhow!("scaled JPEG width overflow"))?;
        let scaled_height = jpeg_scaled_dimension(height as u32, factor)
            .ok_or_else(|| anyhow::anyhow!("scaled JPEG height overflow"))?;
        let pitch = scaled_width
            .checked_mul(3)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| anyhow::anyhow!("scaled JPEG pitch overflow"))?;
        let len = usize::try_from(scaled_width)
            .ok()
            .and_then(|width| {
                usize::try_from(scaled_height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or_else(|| anyhow::anyhow!("scaled JPEG dimensions overflow"))?;
        let mut rgb = vec![0u8; len];
        if (api.tj3_decompress8)(
            handle,
            bytes.as_ptr(),
            bytes.len(),
            rgb.as_mut_ptr(),
            pitch,
            TJPF_RGB,
        ) != 0
        {
            anyhow::bail!("{}", turbojpeg_error(api, handle));
        }
        let image = RgbImage::from_raw(scaled_width, scaled_height, rgb)
            .ok_or_else(|| anyhow::anyhow!("invalid TurboJPEG RGB output dimensions"))?;
        Ok(TurboJpegScaledImage {
            image,
            source_width: width as u32,
            source_height: height as u32,
        })
    }
}

#[cfg(any(windows, test))]
fn jpeg_scaled_dimension(dimension: u32, factor: JpegScalingFactor) -> Option<u32> {
    if factor.num <= 0 || factor.denom <= 0 {
        return None;
    }
    let numerator = u64::from(dimension).checked_mul(factor.num as u64)?;
    let scaled = numerator
        .checked_add(factor.denom as u64 - 1)?
        .checked_div(factor.denom as u64)?;
    u32::try_from(scaled).ok()
}

#[cfg(any(windows, test))]
fn select_jpeg_scaling_factor(
    width: u32,
    height: u32,
    target: u32,
    factors: &[JpegScalingFactor],
) -> JpegScalingFactor {
    let unscaled = JpegScalingFactor { num: 1, denom: 1 };
    if target == 0 || width <= target || height <= target {
        return unscaled;
    }
    factors
        .iter()
        .copied()
        .filter(|factor| factor.num > 0 && factor.denom > 0 && factor.num <= factor.denom)
        .filter_map(|factor| {
            let scaled_width = jpeg_scaled_dimension(width, factor)?;
            let scaled_height = jpeg_scaled_dimension(height, factor)?;
            (scaled_width >= target && scaled_height >= target)
                .then_some((u64::from(scaled_width) * u64::from(scaled_height), factor))
        })
        .min_by_key(|(pixels, _)| *pixels)
        .map(|(_, factor)| factor)
        .unwrap_or(unscaled)
}

fn fingerprint_from_normalized_luma_profiled(
    path: &Path,
    sha256: String,
    luma: NormalizedLumaImage,
    mut profiler: Option<&mut EmbeddingProfiler>,
) -> ImageFingerprint {
    let started = Instant::now();
    let luma32 = resize_luma_to_square(&luma.pixels, luma.width, luma.height, PHASH_SIZE);
    let phash = phash64_hex_from_luma32(&luma32);
    let blur_pixels = resize_luma_for_blur(&luma.pixels, luma.width, luma.height);
    let blur_score = image_blur_score_from_luma(&blur_pixels.pixels, blur_pixels.width as usize);
    let source_class = classify_source_path(path);
    let quality_flags =
        image_quality_flags(path, luma.width, luma.height, blur_score, &source_class);
    if let Some(profiler) = profiler.as_deref_mut() {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_fingerprint_core_ms, started);
    }
    let started = Instant::now();
    let tile_hashes = tile_hashes_from_luma(&luma);
    if let Some(profiler) = profiler {
        EmbeddingProfiler::add_ms(&mut profiler.report.prepare_tile_hash_ms, started);
    }
    ImageFingerprint {
        sha256,
        phash,
        phash_algo: PHASH_ALGO_V2.to_string(),
        width: luma.width,
        height: luma.height,
        blur_score,
        blur_algo: BLUR_ALGO_V2.to_string(),
        quality_flags,
        source_class,
        detected_format: luma.detected_format,
        has_alpha: luma.has_alpha,
        orientation_applied: luma.orientation_applied,
        fingerprint_version: FINGERPRINT_VERSION_V2.to_string(),
        tile_hashes,
    }
}

fn load_dynamic_image_with_orientation(bytes: &[u8]) -> anyhow::Result<(DynamicImage, bool)> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)?;
    let orientation_applied = orientation != image::metadata::Orientation::NoTransforms;
    if orientation_applied {
        image.apply_orientation(orientation);
    }
    Ok((image, orientation_applied))
}

fn jpeg_orientation_requires_dynamic_path(bytes: &[u8]) -> bool {
    let Ok(mut decoder) = image::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes)) else {
        return false;
    };
    decoder
        .orientation()
        .map(|orientation| orientation != image::metadata::Orientation::NoTransforms)
        .unwrap_or(false)
}

fn normalized_luma_from_dynamic_image(
    path: &Path,
    bytes: &[u8],
    image: &DynamicImage,
    orientation_applied: bool,
) -> NormalizedLumaImage {
    let (width, height) = image.dimensions();
    let has_alpha = image.has_alpha();
    let mut pixels = Vec::with_capacity((width as usize).saturating_mul(height as usize));
    for y in 0..height {
        for x in 0..width {
            let pixel = image.get_pixel(x, y).0;
            let alpha = pixel[3] as u32;
            let r = composite_channel_on_white(pixel[0], alpha);
            let g = composite_channel_on_white(pixel[1], alpha);
            let b = composite_channel_on_white(pixel[2], alpha);
            pixels.push(rgb_to_luma_u8(r, g, b));
        }
    }
    NormalizedLumaImage {
        width,
        height,
        pixels,
        detected_format: image_format_label(bytes, path),
        has_alpha,
        orientation_applied,
    }
}

fn composite_channel_on_white(value: u8, alpha: u32) -> u8 {
    (((value as u32 * alpha) + (255 * (255 - alpha)) + 127) / 255) as u8
}

fn rgb_to_luma_u8(r: u8, g: u8, b: u8) -> u8 {
    let luma = ((r as u32 * 19_595) + (g as u32 * 38_470) + (b as u32 * 7_471) + 32_768) >> 16;
    luma as u8
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_bytes(&hasher.finalize())
}

fn image_blur_score(image: &DynamicImage) -> f64 {
    let pixels = luma32_pixels(image);
    image_blur_score_from_luma(&pixels, PHASH_SIZE_USIZE)
}

fn image_blur_score_from_luma(pixels: &[u8], width: usize) -> f64 {
    if width == 0 || pixels.len() < width {
        return 0.0;
    }
    let height = pixels.len() / width;
    if height == 0 {
        return 0.0;
    }
    let mut total = 0u64;
    let mut count = 0u64;
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            let value = pixels[idx] as i32;
            if x + 1 < width {
                total += (value - pixels[idx + 1] as i32).unsigned_abs() as u64;
                count += 1;
            }
            if y + 1 < height {
                total += (value - pixels[idx + width] as i32).unsigned_abs() as u64;
                count += 1;
            }
        }
    }
    if count == 0 {
        0.0
    } else {
        total as f64 / count as f64
    }
}

fn luma32_pixels(image: &DynamicImage) -> Vec<u8> {
    let luma = normalized_luma_from_dynamic_image(Path::new(""), &[], image, false);
    resize_luma_to_square(&luma.pixels, luma.width, luma.height, PHASH_SIZE)
}

fn resize_luma_to_square(pixels: &[u8], width: u32, height: u32, size: u32) -> Vec<u8> {
    if width == 0 || height == 0 || pixels.len() < (width as usize).saturating_mul(height as usize)
    {
        return vec![0; (size as usize).saturating_mul(size as usize)];
    }
    let Some(gray) = GrayImage::from_raw(width, height, pixels.to_vec()) else {
        return vec![0; (size as usize).saturating_mul(size as usize)];
    };
    image::imageops::resize(&gray, size, size, image::imageops::FilterType::Triangle).into_raw()
}

struct BlurInput {
    width: u32,
    pixels: Vec<u8>,
}

fn resize_luma_for_blur(pixels: &[u8], width: u32, height: u32) -> BlurInput {
    if width == 0 || height == 0 || pixels.len() < (width as usize).saturating_mul(height as usize)
    {
        return BlurInput {
            width: 0,
            pixels: Vec::new(),
        };
    }
    let max_side = width.max(height);
    if max_side <= BLUR_INPUT_MAX_SIDE {
        return BlurInput {
            width,
            pixels: pixels.to_vec(),
        };
    }
    let scale = BLUR_INPUT_MAX_SIDE as f64 / max_side as f64;
    let target_width = ((width as f64 * scale).round() as u32).max(1);
    let target_height = ((height as f64 * scale).round() as u32).max(1);
    let Some(gray) = GrayImage::from_raw(width, height, pixels.to_vec()) else {
        return BlurInput {
            width: 0,
            pixels: Vec::new(),
        };
    };
    let resized = image::imageops::resize(
        &gray,
        target_width,
        target_height,
        image::imageops::FilterType::Triangle,
    );
    BlurInput {
        width: target_width,
        pixels: resized.into_raw(),
    }
}

fn tile_hashes_from_luma(luma: &NormalizedLumaImage) -> Vec<TileHash> {
    tile_regions()
        .into_iter()
        .enumerate()
        .map(|(idx, (tile_set, x0, y0, x1, y1))| {
            let tile_pixels =
                resize_luma_region_to_square(&luma.pixels, luma.width, luma.height, x0, y0, x1, y1);
            TileHash {
                tile_algo: TILE_ALGO_V1.to_string(),
                tile_set: tile_set.to_string(),
                tile_index: idx as i64,
                x0,
                y0,
                x1,
                y1,
                hash_hex: phash64_hex_from_luma32(&tile_pixels),
            }
        })
        .collect()
}

fn tile_regions() -> Vec<(&'static str, f64, f64, f64, f64)> {
    let mut regions = Vec::with_capacity(21);
    regions.push(("full", 0.0, 0.0, 1.0, 1.0));
    for gy in 0..2 {
        for gx in 0..2 {
            regions.push((
                "grid_2x2",
                gx as f64 / 2.0,
                gy as f64 / 2.0,
                (gx + 1) as f64 / 2.0,
                (gy + 1) as f64 / 2.0,
            ));
        }
    }
    for gy in 0..3 {
        for gx in 0..3 {
            regions.push((
                "grid_3x3",
                gx as f64 / 3.0,
                gy as f64 / 3.0,
                (gx + 1) as f64 / 3.0,
                (gy + 1) as f64 / 3.0,
            ));
        }
    }
    regions.push(("center_70", 0.15, 0.15, 0.85, 0.85));
    regions.extend_from_slice(&[
        ("strip_left", 0.0, 0.0, 1.0 / 3.0, 1.0),
        ("strip_center", 1.0 / 3.0, 0.0, 2.0 / 3.0, 1.0),
        ("strip_right", 2.0 / 3.0, 0.0, 1.0, 1.0),
        ("strip_top", 0.0, 0.0, 1.0, 1.0 / 3.0),
        ("strip_middle", 0.0, 1.0 / 3.0, 1.0, 2.0 / 3.0),
        ("strip_bottom", 0.0, 2.0 / 3.0, 1.0, 1.0),
    ]);
    regions
}

fn resize_luma_region_to_square(
    pixels: &[u8],
    width: u32,
    height: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
) -> Vec<u8> {
    if width == 0 || height == 0 || pixels.len() < (width as usize).saturating_mul(height as usize)
    {
        return vec![0; PHASH_SIZE_USIZE * PHASH_SIZE_USIZE];
    }
    let Some(gray) = GrayImage::from_raw(width, height, pixels.to_vec()) else {
        return vec![0; PHASH_SIZE_USIZE * PHASH_SIZE_USIZE];
    };
    let left = ((x0.clamp(0.0, 1.0) * width as f64).floor() as u32).min(width - 1);
    let top = ((y0.clamp(0.0, 1.0) * height as f64).floor() as u32).min(height - 1);
    let right = ((x1.clamp(0.0, 1.0) * width as f64).ceil() as u32).clamp(left + 1, width);
    let bottom = ((y1.clamp(0.0, 1.0) * height as f64).ceil() as u32).clamp(top + 1, height);
    let crop = image::imageops::crop_imm(&gray, left, top, right - left, bottom - top).to_image();
    image::imageops::resize(
        &crop,
        PHASH_SIZE,
        PHASH_SIZE,
        image::imageops::FilterType::Triangle,
    )
    .into_raw()
}

fn classify_source_path(path: &Path) -> String {
    let text = normalized_path_text(path);
    if text.contains("/emoji/") || text.contains("emoji") || text.contains("face") {
        "emoji".to_string()
    } else if text.contains("/avatar/") || text.contains("avatar") {
        "avatar".to_string()
    } else if text.contains("/filerecv/") {
        "file_recv".to_string()
    } else if text.contains("/nt_data/pic/") {
        "chat_pic".to_string()
    } else if text.contains("/nt_data/video/") || text.contains("/video/") {
        "video_thumbnail".to_string()
    } else if text.contains("/image/") {
        "legacy_image".to_string()
    } else if text.contains("thumb") {
        "thumbnail".to_string()
    } else {
        "image".to_string()
    }
}

fn image_quality_flags(
    path: &Path,
    width: u32,
    height: u32,
    _blur_score: f64,
    source_class: &str,
) -> String {
    let mut flags = Vec::new();
    if width.min(height) < TINY_IMAGE_EDGE {
        flags.push("tiny");
    }
    if width.min(height) < SMALL_IMAGE_EDGE {
        flags.push("small");
    }
    if matches!(
        source_class,
        "avatar" | "emoji" | "thumbnail" | "video_thumbnail"
    ) || normalized_path_text(path).contains("thumb")
    {
        flags.push("thumbnail_like");
    }
    flags.join(",")
}

fn normalized_path_text(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn fingerprint_image_for_query(
    path: &Path,
    mode: &QueryMode,
) -> anyhow::Result<(ImageFingerprint, Option<DynamicImage>)> {
    if *mode == QueryMode::Exact {
        return Ok((exact_query_fingerprint(path)?, None));
    }
    fingerprint_image_for_index(path, mode.wants_semantic() || mode.wants_copy())
}

fn exact_query_fingerprint(path: &Path) -> anyhow::Result<ImageFingerprint> {
    Ok(ImageFingerprint {
        sha256: sha256_file(path)?,
        phash: String::new(),
        phash_algo: String::new(),
        width: 0,
        height: 0,
        blur_score: 0.0,
        blur_algo: String::new(),
        quality_flags: String::new(),
        source_class: String::new(),
        detected_format: String::new(),
        has_alpha: false,
        orientation_applied: false,
        fingerprint_version: String::new(),
        tile_hashes: Vec::new(),
    })
}

fn hydrate_query_fingerprint_from_matches(
    fingerprint: &mut ImageFingerprint,
    matches: &[IndexedImage],
) {
    if !fingerprint.phash.is_empty() {
        return;
    }
    let Some(item) = matches
        .iter()
        .find(|item| item.phash.as_deref().unwrap_or("").len() == 16)
    else {
        return;
    };
    fingerprint.phash = item.phash.clone().unwrap_or_default();
    fingerprint.phash_algo = item.phash_algo.clone().unwrap_or_default();
    fingerprint.width = item.width.unwrap_or(0);
    fingerprint.height = item.height.unwrap_or(0);
}

fn ensure_query_fingerprint(
    path: &Path,
    fingerprint: &mut ImageFingerprint,
    query_image: &mut Option<DynamicImage>,
    keep_image: bool,
) -> anyhow::Result<()> {
    if !fingerprint.phash.is_empty() && (!keep_image || query_image.is_some()) {
        return Ok(());
    }
    let (full, image) = fingerprint_image_for_index(path, keep_image)?;
    *fingerprint = full;
    if keep_image {
        *query_image = image;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = fs_file_open_for_host(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 64];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_bytes(&hasher.finalize()))
}

fn phash64_hex(image: &image::DynamicImage) -> String {
    let pixels = luma32_pixels(image);
    phash64_hex_from_luma32(&pixels)
}

fn phash64_hex_from_luma32(pixels: &[u8]) -> String {
    let cos_table = dct_cos_table();
    let mut row_coeffs = [[0f64; PHASH_SIZE_USIZE]; PHASH_LOW];
    for u in 0..PHASH_LOW {
        for (y, coefficient) in row_coeffs[u].iter_mut().enumerate() {
            let row_start = y * PHASH_SIZE_USIZE;
            let mut sum = 0f64;
            for x in 0..PHASH_SIZE_USIZE {
                sum += pixels[row_start + x] as f64 * cos_table[u][x];
            }
            *coefficient = sum;
        }
    }
    let mut coeffs = [[0f64; PHASH_LOW]; PHASH_LOW];
    for u in 0..PHASH_LOW {
        for v in 0..PHASH_LOW {
            let mut sum = 0f64;
            for y in 0..PHASH_SIZE_USIZE {
                sum += row_coeffs[u][y] * cos_table[v][y];
            }
            coeffs[u][v] = sum;
        }
    }
    let mut vals = Vec::with_capacity(PHASH_LOW * PHASH_LOW - 1);
    for (u, row) in coeffs.iter().enumerate() {
        for (v, coeff) in row.iter().enumerate() {
            if u != 0 || v != 0 {
                vals.push(*coeff);
            }
        }
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = vals[vals.len() / 2];
    let mut bits = 0u64;
    let mut pos = 0u32;
    for (u, row) in coeffs.iter().enumerate() {
        for (v, coeff) in row.iter().enumerate() {
            if u == 0 && v == 0 {
                continue;
            }
            if *coeff > median {
                bits |= 1u64 << pos;
            }
            pos += 1;
        }
    }
    format!("{bits:016x}")
}

fn dct_cos_table() -> &'static [[f64; PHASH_SIZE_USIZE]; PHASH_LOW] {
    static TABLE: OnceLock<[[f64; PHASH_SIZE_USIZE]; PHASH_LOW]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [[0f64; PHASH_SIZE_USIZE]; PHASH_LOW];
        let n = PHASH_SIZE as f64;
        for (u, row) in table.iter_mut().enumerate() {
            for (x, value) in row.iter_mut().enumerate() {
                *value = (((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI) / (2.0 * n)).cos();
            }
        }
        table
    })
}

fn exact_matches(
    con: &Connection,
    sha256: &str,
    limit: usize,
) -> anyhow::Result<Vec<IndexedImage>> {
    let limit = normalize_query_limit(limit);
    let mut stmt = con.prepare_cached(
        "select path, sha256_hex, phash_hex, phash_algo, width, height, blur_score, blur_algo, quality_flags, source_class from image_assets where stale=0 and sha256_hex=?1 order by path limit ?2",
    )?;
    let rows = stmt.query_map(params![sha256, limit as i64], image_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn near_hash_matches(
    con: &Connection,
    phash: &str,
    phash_algo: &str,
    threshold: u32,
    limit: usize,
) -> anyhow::Result<Vec<(IndexedImage, u32)>> {
    let limit = normalize_query_limit(limit);
    let query = u64::from_str_radix(phash, 16)?;
    let mut stmt = con.prepare_cached(
        "select phash_hex, path, sha256_hex, phash_algo, width, height, blur_score, blur_algo, quality_flags, source_class from image_assets where stale=0 and phash_algo=?1 and phash_hex is not null and phash_hex!=''",
    )?;
    let mut out: Vec<(IndexedImage, u32)> = Vec::with_capacity(limit);
    let mut rows = stmt.query(params![phash_algo])?;
    while let Some(row) = rows.next()? {
        let phash_value = row.get::<_, String>(0)?;
        let Some(item_hash) = u64::from_str_radix(&phash_value, 16).ok() else {
            continue;
        };
        let distance = (query ^ item_hash).count_ones();
        if distance > threshold {
            continue;
        }
        if let Some(worst) = out.last() {
            if out.len() >= limit && distance > worst.1 {
                continue;
            }
        }
        let item = image_from_near_hash_row(row, phash_value)?;
        push_top_near_hash(&mut out, (item, distance), limit);
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.path.cmp(&b.0.path)));
    Ok(out)
}

fn tile_hash_matches(
    con: &Connection,
    query_tiles: &[TileHash],
    threshold: u32,
    limit: usize,
) -> anyhow::Result<Vec<(IndexedImage, u32)>> {
    let limit = normalize_query_limit(limit);
    if query_tiles.is_empty() {
        return Ok(Vec::new());
    }
    let mut query_hashes = Vec::with_capacity(query_tiles.len());
    for tile in query_tiles {
        if tile.tile_algo != TILE_ALGO_V1 {
            continue;
        }
        if let Ok(value) = u64::from_str_radix(&tile.hash_hex, 16) {
            query_hashes.push(value);
        }
    }
    if query_hashes.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = con.prepare_cached(
        r#"
        select t.hash_hex, t.tile_algo,
               a.path, a.sha256_hex, a.phash_hex, a.phash_algo, a.width, a.height,
               a.blur_score, a.blur_algo, a.quality_flags, a.source_class
        from image_asset_tile_hashes t
        join image_assets a on a.id=t.asset_id
        where a.stale=0 and t.tile_algo=?1 and t.hash_hex!=''
        "#,
    )?;
    let mut by_path: HashMap<String, (IndexedImage, u32, usize)> = HashMap::new();
    let mut rows = stmt.query(params![TILE_ALGO_V1])?;
    while let Some(row) = rows.next()? {
        let hash_hex = row.get::<_, String>(0)?;
        let Some(tile_hash) = u64::from_str_radix(&hash_hex, 16).ok() else {
            continue;
        };
        let Some(distance) = query_hashes
            .iter()
            .map(|query| (query ^ tile_hash).count_ones())
            .min()
        else {
            continue;
        };
        if distance > threshold {
            continue;
        }
        let mut item = image_from_tile_hash_row(row)?;
        item.tile_algo = Some(row.get::<_, String>(1)?);
        let path_key = item.path.to_string_lossy().to_string();
        by_path
            .entry(path_key)
            .and_modify(|(_, best, count)| {
                *best = (*best).min(distance);
                *count += 1;
            })
            .or_insert((item, distance, 1));
    }
    let mut out = by_path
        .into_values()
        .map(|(mut item, distance, matched)| {
            item.matched_tile_count = Some(matched);
            (item, distance)
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| b.0.matched_tile_count.cmp(&a.0.matched_tile_count))
            .then_with(|| a.0.path.cmp(&b.0.path))
    });
    out.truncate(limit);
    Ok(out)
}

fn push_top_near_hash(
    out: &mut Vec<(IndexedImage, u32)>,
    candidate: (IndexedImage, u32),
    limit: usize,
) {
    if limit == 0 {
        return;
    }
    if out.len() >= limit {
        if let Some(worst) = out.last() {
            if !near_hash_candidate_better(&candidate, worst) {
                return;
            }
        }
        out.pop();
    }
    insert_top_near_hash_sorted(out, candidate);
}

fn insert_top_near_hash_sorted(out: &mut Vec<(IndexedImage, u32)>, candidate: (IndexedImage, u32)) {
    let pos = out
        .iter()
        .position(|existing| near_hash_candidate_better(&candidate, existing))
        .unwrap_or(out.len());
    out.insert(pos, candidate);
}

fn near_hash_candidate_better(a: &(IndexedImage, u32), b: &(IndexedImage, u32)) -> bool {
    a.1 < b.1 || (a.1 == b.1 && a.0.path < b.0.path)
}

fn image_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedImage> {
    Ok(IndexedImage {
        path: PathBuf::from(row.get::<_, String>(0)?),
        sha256: row.get(1)?,
        phash: row.get(2)?,
        phash_algo: phash_algo_opt(row.get(3)?),
        width: row.get::<_, Option<i64>>(4)?.map(|v| v as u32),
        height: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        blur_score: row.get(6)?,
        blur_algo: non_empty_opt(row.get(7)?),
        quality_flags: row.get(8)?,
        source_class: row.get(9)?,
        tile_algo: None,
        matched_tile_count: None,
    })
}

fn image_from_near_hash_row(
    row: &rusqlite::Row<'_>,
    phash: String,
) -> rusqlite::Result<IndexedImage> {
    Ok(IndexedImage {
        path: PathBuf::from(row.get::<_, String>(1)?),
        sha256: row.get(2)?,
        phash: Some(phash),
        phash_algo: phash_algo_opt(row.get(3)?),
        width: row.get::<_, Option<i64>>(4)?.map(|v| v as u32),
        height: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        blur_score: row.get(6)?,
        blur_algo: non_empty_opt(row.get(7)?),
        quality_flags: row.get(8)?,
        source_class: row.get(9)?,
        tile_algo: None,
        matched_tile_count: None,
    })
}

fn image_from_tile_hash_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedImage> {
    Ok(IndexedImage {
        path: PathBuf::from(row.get::<_, String>(2)?),
        sha256: row.get(3)?,
        phash: row.get(4)?,
        phash_algo: phash_algo_opt(row.get(5)?),
        width: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
        height: row.get::<_, Option<i64>>(7)?.map(|v| v as u32),
        blur_score: row.get(8)?,
        blur_algo: non_empty_opt(row.get(9)?),
        quality_flags: row.get(10)?,
        source_class: row.get(11)?,
        tile_algo: None,
        matched_tile_count: None,
    })
}

fn non_empty_opt(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn phash_algo_opt(value: String) -> Option<String> {
    if value.is_empty() {
        Some(PHASH_ALGO_LEGACY.to_string())
    } else {
        Some(value)
    }
}

fn push_result(
    out: &mut Vec<QueryResult>,
    match_kind: &str,
    score: f64,
    distance: Option<u32>,
    item: IndexedImage,
    limit: usize,
) {
    if limit == 0 {
        return;
    }
    let candidate = query_result_from_item(match_kind, score, distance, item);
    if out.len() >= limit {
        if let Some(worst) = out.last() {
            if !query_result_better(&candidate, worst) {
                return;
            }
        }
    }
    if let Some(existing) = out
        .iter()
        .position(|existing| existing.path == candidate.path)
    {
        let new_priority = match_kind_priority(match_kind);
        let existing_priority = match_kind_priority(&out[existing].match_kind);
        let should_replace = new_priority > existing_priority
            || (new_priority == existing_priority && score > out[existing].score);
        if should_replace {
            out.remove(existing);
            insert_query_result_sorted(out, candidate);
        }
        return;
    }
    if out.len() >= limit {
        out.pop();
    }
    insert_query_result_sorted(out, candidate);
}

fn insert_query_result_sorted(out: &mut Vec<QueryResult>, candidate: QueryResult) {
    let pos = out
        .iter()
        .position(|existing| query_result_better(&candidate, existing))
        .unwrap_or(out.len());
    out.insert(pos, candidate);
}

fn result_buffer_excludes_kind(results: &[QueryResult], limit: usize, match_kind: &str) -> bool {
    if results.len() < limit {
        return false;
    }
    results
        .last()
        .map(|worst| match_kind_priority(&worst.match_kind) > match_kind_priority(match_kind))
        .unwrap_or(false)
}

fn query_result_better(a: &QueryResult, b: &QueryResult) -> bool {
    query_result_sort_cmp(a, b) == std::cmp::Ordering::Less
}

fn query_result_sort_cmp(a: &QueryResult, b: &QueryResult) -> std::cmp::Ordering {
    match_kind_priority(&b.match_kind)
        .cmp(&match_kind_priority(&a.match_kind))
        .then_with(|| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| a.path.cmp(&b.path))
}

fn match_kind_priority(match_kind: &str) -> u8 {
    match match_kind {
        "exact" => 4,
        "same_sscd" | "copy_sscd" => 3,
        "near_hash" => 2,
        "tile_hash" => 2,
        "semantic_clip" => 1,
        _ => 0,
    }
}

fn sscd_match_kind(score: f64) -> &'static str {
    if score >= SSCD_SAME_IMAGE_THRESHOLD {
        "same_sscd"
    } else {
        "copy_sscd"
    }
}

fn query_result_from_item(
    match_kind: &str,
    score: f64,
    distance: Option<u32>,
    item: IndexedImage,
) -> QueryResult {
    QueryResult {
        match_kind: match_kind.to_string(),
        score,
        distance,
        href: asset_href(&item.path),
        name: item
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
        path: item.path.to_string_lossy().to_string(),
        width: item.width,
        height: item.height,
        sha256: item.sha256,
        phash: item.phash,
        phash_algo: item.phash_algo,
        blur_score: item.blur_score,
        blur_algo: item.blur_algo,
        quality_flags: item.quality_flags,
        source_class: item.source_class,
        match_source: match_kind.to_string(),
        tile_algo: item.tile_algo,
        embedding_model: None,
        local_algo: None,
        matched_tile_count: item.matched_tile_count,
        local_inliers: None,
    }
}

fn open_existing_manifest(
    root: &Path,
    account: &str,
    manifest_path: Option<&Path>,
) -> anyhow::Result<Connection> {
    let manifest = manifest_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_manifest_path(root, account));
    if !manifest.is_file() {
        anyhow::bail!(
            "image index manifest is missing: {}; run image-index build first",
            manifest.display()
        );
    }
    let con = Connection::open(manifest)?;
    init_schema(&con)?;
    Ok(con)
}

fn count_i64(con: &Connection, sql: &str) -> anyhow::Result<i64> {
    Ok(con.query_row(sql, [], |row| row.get(0))?)
}

fn embedding_stats(con: &Connection) -> anyhow::Result<EmbeddingStats> {
    Ok(EmbeddingStats {
        rows: count_i64(con, "select count(*) from image_embeddings")?,
        active_rows: count_i64(
            con,
            "select count(*) from image_embeddings e join image_assets a on a.path=e.path where a.stale=0",
        )?,
        active_clip_rows: count_i64(
            con,
            "select count(*) from image_embeddings e join image_assets a on a.path=e.path where a.stale=0 and e.kind='clip'",
        )?,
        active_sscd_rows: count_i64(
            con,
            "select count(*) from image_embeddings e join image_assets a on a.path=e.path where a.stale=0 and e.kind='sscd'",
        )?,
        active_rows_with_sketch: count_i64(
            con,
            "select count(*) from image_embeddings e join image_assets a on a.path=e.path where a.stale=0 and length(e.sketch64_hex)=16 and e.bucket12 is not null and e.bucket12>=0",
        )?,
        active_rows_missing_sketch: count_i64(
            con,
            "select count(*) from image_embeddings e join image_assets a on a.path=e.path where a.stale=0 and not (length(e.sketch64_hex)=16 and e.bucket12 is not null and e.bucket12>=0)",
        )?,
        active_buckets: count_i64(
            con,
            "select count(distinct e.bucket12) from image_embeddings e join image_assets a on a.path=e.path where a.stale=0 and length(e.sketch64_hex)=16 and e.bucket12 is not null and e.bucket12>=0",
        )?,
        active_images_missing_clip: count_i64(
            con,
            r#"
            select count(*)
            from image_assets a
            where a.stale=0
              and (a.error is null or a.error='')
              and not exists (
                  select 1 from image_embeddings e
                  where e.path=a.path and e.kind='clip'
                    and length(e.sketch64_hex)=16
                    and e.bucket12 is not null
                    and e.bucket12>=0
              )
            "#,
        )?,
        active_images_missing_sscd: count_i64(
            con,
            r#"
            select count(*)
            from image_assets a
            where a.stale=0
              and (a.error is null or a.error='')
              and not exists (
                  select 1 from image_embeddings e
                  where e.path=a.path and e.kind='sscd'
                    and length(e.sketch64_hex)=16
                    and e.bucket12 is not null
                    and e.bucket12>=0
              )
            "#,
        )?,
    })
}

fn asset_quality_stats(con: &Connection) -> anyhow::Result<AssetQualityStats> {
    Ok(AssetQualityStats {
        active_images: count_i64(con, "select count(*) from image_assets where stale=0")?,
        duplicate_sha_groups: count_i64(
            con,
            "select count(*) from (select sha256_hex from image_assets where stale=0 and sha256_hex!='' group by sha256_hex having count(*)>1)",
        )?,
        duplicate_sha_files: count_i64(
            con,
            "select coalesce(sum(n), 0) from (select count(*) n from image_assets where stale=0 and sha256_hex!='' group by sha256_hex having count(*)>1)",
        )?,
        exact_phash_groups: count_i64(
            con,
            "select count(*) from (select phash_hex from image_assets where stale=0 and phash_hex is not null and phash_hex!='' group by phash_hex having count(*)>1)",
        )?,
        exact_phash_files: count_i64(
            con,
            "select coalesce(sum(n), 0) from (select count(*) n from image_assets where stale=0 and phash_hex is not null and phash_hex!='' group by phash_hex having count(*)>1)",
        )?,
        blurry_files: count_i64(
            con,
            "select count(*) from image_assets where stale=0 and instr(',' || quality_flags || ',', ',blurry,')>0",
        )?,
        small_files: count_i64(
            con,
            "select count(*) from image_assets where stale=0 and instr(',' || quality_flags || ',', ',small,')>0",
        )?,
        tiny_files: count_i64(
            con,
            "select count(*) from image_assets where stale=0 and instr(',' || quality_flags || ',', ',tiny,')>0",
        )?,
        thumbnail_like_files: count_i64(
            con,
            "select count(*) from image_assets where stale=0 and instr(',' || quality_flags || ',', ',thumbnail_like,')>0",
        )?,
        manifest_pending_files: count_i64(
            con,
            "select count(*) from image_assets where stale=0 and instr(',' || quality_flags || ',', ',manifest_pending,')>0",
        )?,
    })
}

fn decode_asset_path(root: &Path, asset: &str) -> anyhow::Result<PathBuf> {
    let encoded = asset
        .trim()
        .trim_start_matches("/asset/")
        .split('/')
        .next()
        .unwrap_or("");
    let padding = "=".repeat((4 - encoded.len() % 4) % 4);
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded.as_bytes())
        .or_else(|_| {
            base64::engine::general_purpose::URL_SAFE
                .decode(format!("{encoded}{padding}").as_bytes())
        })?;
    let path = PathBuf::from(String::from_utf8_lossy(&decoded).to_string());
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let path = path.canonicalize()?;
    if !path.starts_with(root) {
        anyhow::bail!("asset outside root");
    }
    Ok(path)
}

fn model_status(options: &BuildOptions) -> ModelStatus {
    let clip_dir = resolve_clip_model_dir(options);
    let sscd_path = resolve_sscd_model_path(options);
    let semantic_descriptor = clip_status(&clip_dir);
    let copy_descriptor = sscd_status(&sscd_path);
    ModelStatus {
        backend: options.backend.clone(),
        clip_model: options.clip_model.clone(),
        sscd_model: options.sscd_model.clone(),
        execution_provider: options.execution_provider.clone(),
        execution_provider_status: onnx_execution_provider_status(options),
        model_dir: clip_dir.as_ref().map(|path| path.display().to_string()),
        sscd_model_path: sscd_path.as_ref().map(|path| path.display().to_string()),
        exact_hash: "available:sha256".to_string(),
        near_hash: "available:phash64".to_string(),
        copy_descriptor,
        semantic_descriptor,
    }
}

fn default_clip_model_dir(root: &Path) -> Option<PathBuf> {
    if let Ok(path) = env::var("QQ_ANALYZER_CLIP_MODEL_DIR") {
        if !path.trim().is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let fallback = root
        .join("qq-analyzer")
        .join("output")
        .join("_deps")
        .join("models")
        .join("mobileclip2-s2");
    fallback.is_dir().then_some(fallback)
}

fn resolve_clip_model_dir(options: &BuildOptions) -> Option<PathBuf> {
    if let Some(path) = options.model_dir.clone() {
        return Some(path);
    }
    if let Ok(path) = env::var("QQ_ANALYZER_CLIP_MODEL_DIR") {
        if !path.trim().is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let fallback = options
        .root
        .join("qq-analyzer")
        .join("output")
        .join("_deps")
        .join("models")
        .join(&options.clip_model);
    fallback.is_dir().then_some(fallback)
}

fn default_sscd_model_dir(root: &Path) -> Option<PathBuf> {
    if let Ok(path) = env::var("QQ_ANALYZER_SSCD_MODEL_DIR") {
        if !path.trim().is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let fallback = root
        .join("qq-analyzer")
        .join("output")
        .join("_deps")
        .join("models")
        .join("sscd");
    fallback.is_dir().then_some(fallback)
}

fn resolve_sscd_model_path(options: &BuildOptions) -> Option<PathBuf> {
    let dir = options
        .sscd_model_dir
        .clone()
        .or_else(|| {
            env::var("QQ_ANALYZER_SSCD_MODEL_DIR")
                .ok()
                .map(PathBuf::from)
        })
        .or_else(|| {
            options
                .root
                .join("qq-analyzer")
                .join("output")
                .join("_deps")
                .join("models")
                .join("sscd")
                .is_dir()
                .then(|| {
                    options
                        .root
                        .join("qq-analyzer")
                        .join("output")
                        .join("_deps")
                        .join("models")
                        .join("sscd")
                })
        })?;
    if dir.is_file() {
        return Some(dir);
    }
    let candidates = [
        dir.join(format!("{}.onnx", options.sscd_model)),
        dir.join("sscd_disc_mixup.onnx"),
        dir.join("model.onnx"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(feature = "image-index-sscd")]
fn sscd_ort_profile_path() -> anyhow::Result<Option<PathBuf>> {
    let Some(value) = env::var_os(SSCD_ORT_PROFILE_ENV) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| {
            anyhow::anyhow!(
                "create SSCD ORT profile directory {}: {err}",
                parent.display()
            )
        })?;
    }
    Ok(Some(path))
}

#[cfg(feature = "image-index-clip")]
fn clip_model_key(model: &str, dir: &Path) -> String {
    model_key_with_path_variant(
        "mobileclip2-s2",
        model,
        dir.file_name().and_then(|name| name.to_str()),
    )
}

#[cfg(feature = "image-index-sscd")]
fn sscd_model_key(model: &str, path: &Path) -> String {
    let variant = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| *stem != "model")
        .or_else(|| {
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
        });
    model_key_with_path_variant("sscd_disc_mixup", model, variant)
}

#[cfg(any(feature = "image-index-clip", feature = "image-index-sscd", test))]
fn model_key_with_path_variant(
    default_model: &str,
    requested_model: &str,
    path_variant: Option<&str>,
) -> String {
    let requested = requested_model.trim();
    if !requested.is_empty() && requested != default_model {
        return requested.to_string();
    }
    let fallback = if requested.is_empty() {
        default_model
    } else {
        requested
    };
    let Some(variant) = path_variant
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return fallback.to_string();
    };
    if variant == default_model || variant == "model" {
        fallback.to_string()
    } else {
        variant.to_string()
    }
}

fn clip_unavailable_signal(root: &Path, model_dir: Option<&PathBuf>) -> UnavailableSignal {
    let reason = if cfg!(feature = "image-index-clip") {
        let expected = model_dir
            .cloned()
            .or_else(|| default_clip_model_dir(root))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| {
                root.join("qq-analyzer")
                    .join("output")
                    .join("_deps")
                    .join("models")
                    .join("mobileclip2-s2")
                    .display()
                    .to_string()
            });
        format!(
            "MobileCLIP2 adapter is enabled, but no usable local model directory was found; pass --model-dir or set QQ_ANALYZER_CLIP_MODEL_DIR (expected {expected})"
        )
    } else {
        "MobileCLIP2 adapter is not enabled in this build; rebuild with --features image-index-clip"
            .to_string()
    };
    UnavailableSignal {
        signal: "semantic_descriptor:clip_vec".to_string(),
        reason,
    }
}

fn sscd_unavailable_signal(root: &Path, model_dir: Option<&PathBuf>) -> UnavailableSignal {
    let reason = if cfg!(feature = "image-index-sscd") {
        let expected = model_dir
            .cloned()
            .or_else(|| default_sscd_model_dir(root))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| {
                root.join("qq-analyzer")
                    .join("output")
                    .join("_deps")
                    .join("models")
                    .join("sscd")
                    .join("sscd_disc_mixup.onnx")
                    .display()
                    .to_string()
            });
        format!(
            "SSCD adapter is enabled, but no usable ONNX model was found; pass --sscd-model-dir or set QQ_ANALYZER_SSCD_MODEL_DIR (expected {expected})"
        )
    } else {
        "SSCD adapter is not enabled in this build; rebuild with --features image-index-sscd"
            .to_string()
    };
    UnavailableSignal {
        signal: "copy_descriptor:sscd_vec".to_string(),
        reason,
    }
}

#[cfg(feature = "image-index-clip")]
fn clip_status(model_dir: &Option<PathBuf>) -> String {
    match model_dir {
        Some(dir) if clip_model_dir_complete(dir) => {
            "available:mobileclip2_openclip_onnx_local".to_string()
        }
        Some(dir) => format!("unavailable:clip_model_dir_incomplete:{}", dir.display()),
        None => "unavailable:clip_model_dir_missing".to_string(),
    }
}

#[cfg(not(feature = "image-index-clip"))]
fn clip_status(_model_dir: &Option<PathBuf>) -> String {
    "unavailable:mobileclip2_onnx_adapter_not_enabled".to_string()
}

#[cfg(feature = "image-index-sscd")]
fn sscd_status(model_path: &Option<PathBuf>) -> String {
    match model_path {
        Some(path) => format!("available:sscd_onnx_local:{}", path.display()),
        None => "unavailable:sscd_onnx_model_missing".to_string(),
    }
}

#[cfg(not(feature = "image-index-sscd"))]
fn sscd_status(_model_path: &Option<PathBuf>) -> String {
    "unavailable:sscd_onnx_adapter_not_enabled".to_string()
}

#[cfg(feature = "image-index-clip")]
fn clip_model_dir_complete(dir: &Path) -> bool {
    [
        "visual.onnx",
        "text.onnx",
        "open_clip_config.json",
        "model_config.json",
        "tokenizer.json",
    ]
    .iter()
    .all(|name| dir.join(name).is_file())
}

#[cfg(feature = "image-index-clip")]
struct ClipVisionRuntime {
    model_key: String,
    embedder: open_clip_inference::VisionEmbedder,
    concurrent_gpu_runs: bool,
}

#[cfg(not(feature = "image-index-clip"))]
struct ClipVisionRuntime {
    model_key: String,
}

#[cfg(feature = "image-index-clip")]
struct ClipTextRuntime {
    model_key: String,
    embedder: open_clip_inference::TextEmbedder,
}

#[cfg(not(feature = "image-index-clip"))]
struct ClipTextRuntime {
    model_key: String,
}

#[cfg(feature = "image-index-sscd")]
struct SscdRuntime {
    model_key: String,
    input_name: String,
    output_name: String,
    input_tensor_type: ort::value::TensorElementType,
    output_tensor_type: ort::value::TensorElementType,
    static_input_batch_size: Option<usize>,
    static_output_shape: Option<Vec<usize>>,
    io_binding: std::sync::Mutex<Option<SscdIoBinding>>,
    session: std::sync::Mutex<ort::session::Session>,
    ort_profile_path: Option<PathBuf>,
    ort_profile_ended: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "image-index-sscd")]
struct SscdIoBinding {
    input_shape: Vec<usize>,
    output_shape: Vec<usize>,
    state: SscdIoBindingState,
}

#[cfg(feature = "image-index-sscd")]
enum SscdIoBindingState {
    F32(SscdIoBindingF32),
    F16(SscdIoBindingF16),
}

#[cfg(feature = "image-index-sscd")]
struct SscdIoBindingF32 {
    input_cpu: ort::value::Tensor<f32>,
    input_device: ort::value::Tensor<f32>,
    output_cpu: ort::value::Tensor<f32>,
    binding: ort::session::IoBinding,
}

#[cfg(feature = "image-index-sscd")]
struct SscdIoBindingF16 {
    input_cpu: ort::value::Tensor<half::f16>,
    input_device: ort::value::Tensor<half::f16>,
    output_cpu: ort::value::Tensor<f32>,
    binding: ort::session::IoBinding,
}

#[cfg(feature = "image-index-sscd")]
impl Drop for SscdIoBinding {
    fn drop(&mut self) {
        match &mut self.state {
            SscdIoBindingState::F32(state) => {
                let _ = state.binding.synchronize();
                state.binding.clear();
            }
            SscdIoBindingState::F16(state) => {
                let _ = state.binding.synchronize();
                state.binding.clear();
            }
        }
    }
}

#[cfg(not(feature = "image-index-sscd"))]
struct SscdRuntime {
    model_key: String,
}

#[cfg(feature = "image-index-clip")]
fn load_clip_vision_runtime(options: &BuildOptions) -> anyhow::Result<Option<ClipVisionRuntime>> {
    let Some(model_dir) = resolve_clip_model_dir(options) else {
        return Ok(None);
    };
    if !clip_model_dir_complete(&model_dir) {
        anyhow::bail!("incomplete CLIP model dir: {}", model_dir.display());
    }
    let providers = onnx_execution_providers(options)?;
    let embedder = if providers.is_empty() {
        open_clip_inference::VisionEmbedder::from_local_dir(&model_dir).build()
    } else {
        open_clip_inference::VisionEmbedder::from_local_dir(&model_dir)
            .with_execution_providers(&providers)
            .build()
    }
    .map_err(|err| anyhow::anyhow!("load CLIP vision model {}: {err}", model_dir.display()))?;
    Ok(Some(ClipVisionRuntime {
        model_key: clip_model_key(&options.clip_model, &model_dir),
        embedder,
        concurrent_gpu_runs: concurrent_gpu_runs_enabled_for_provider(&options.execution_provider),
    }))
}

#[cfg(not(feature = "image-index-clip"))]
fn load_clip_vision_runtime(_options: &BuildOptions) -> anyhow::Result<Option<ClipVisionRuntime>> {
    Ok(None)
}

#[cfg(feature = "image-index-clip")]
fn load_clip_text_runtime(options: &BuildOptions) -> anyhow::Result<Option<ClipTextRuntime>> {
    let Some(model_dir) = resolve_clip_model_dir(options) else {
        return Ok(None);
    };
    if !clip_model_dir_complete(&model_dir) {
        anyhow::bail!("incomplete CLIP model dir: {}", model_dir.display());
    }
    let providers = onnx_execution_providers(options)?;
    let embedder = if providers.is_empty() {
        open_clip_inference::TextEmbedder::from_local_dir(&model_dir).build()
    } else {
        open_clip_inference::TextEmbedder::from_local_dir(&model_dir)
            .with_execution_providers(&providers)
            .build()
    }
    .map_err(|err| anyhow::anyhow!("load CLIP text model {}: {err}", model_dir.display()))?;
    Ok(Some(ClipTextRuntime {
        model_key: clip_model_key(&options.clip_model, &model_dir),
        embedder,
    }))
}

#[cfg(not(feature = "image-index-clip"))]
fn load_clip_text_runtime(_options: &BuildOptions) -> anyhow::Result<Option<ClipTextRuntime>> {
    Ok(None)
}

#[cfg(feature = "image-index-sscd")]
fn load_sscd_runtime(options: &BuildOptions) -> anyhow::Result<Option<SscdRuntime>> {
    let Some(model_path) = resolve_sscd_model_path(options) else {
        return Ok(None);
    };
    let providers = onnx_execution_providers(options)?;
    let ort_profile_path = sscd_ort_profile_path()?;
    let builder = ort::session::Session::builder()?;
    let sscd_threads =
        sscd_ort_intra_threads()?.or(if providers.is_empty() { None } else { Some(1) });
    let builder = if let Some(threads) = sscd_threads {
        builder
            .with_intra_threads(threads)
            .map_err(|err| anyhow::anyhow!("configure SSCD ORT intra-op threads: {err}"))?
    } else {
        builder
    };
    let builder = if providers.is_empty() {
        builder
    } else {
        builder
            .with_execution_providers(&providers)
            .map_err(|err| anyhow::anyhow!("register SSCD execution provider: {err}"))?
    };
    let builder = if let Some(path) = ort_profile_path.as_ref() {
        builder
            .with_profiling(path)
            .map_err(|err| anyhow::anyhow!("enable SSCD ORT profiling {}: {err}", path.display()))?
    } else {
        builder
    };
    let session = builder
        .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
        .map_err(|err| anyhow::anyhow!("configure SSCD graph optimization: {err}"))?
        .commit_from_file(&model_path)
        .map_err(|err| anyhow::anyhow!("load SSCD model {}: {err}", model_path.display()))?;
    let input_name = session
        .inputs()
        .first()
        .map(|input| input.name().to_string())
        .ok_or_else(|| anyhow::anyhow!("SSCD model {} has no inputs", model_path.display()))?;
    let output_name = session
        .outputs()
        .first()
        .map(|output| output.name().to_string())
        .ok_or_else(|| anyhow::anyhow!("SSCD model {} has no outputs", model_path.display()))?;
    let input_tensor_type = session
        .inputs()
        .first()
        .and_then(|input| input.dtype().tensor_type())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "SSCD model {} input type is not a tensor",
                model_path.display()
            )
        })?;
    let output_tensor_type = session
        .outputs()
        .first()
        .and_then(|output| output.dtype().tensor_type())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "SSCD model {} output type is not a tensor",
                model_path.display()
            )
        })?;
    let static_input_batch_size = session
        .inputs()
        .first()
        .and_then(|input| static_tensor_shape(input.dtype()))
        .and_then(|shape| shape.first().copied());
    let static_output_shape = session
        .outputs()
        .first()
        .and_then(|output| static_tensor_shape(output.dtype()));
    Ok(Some(SscdRuntime {
        model_key: sscd_model_key(&options.sscd_model, &model_path),
        input_name,
        output_name,
        input_tensor_type,
        output_tensor_type,
        static_input_batch_size,
        static_output_shape,
        io_binding: std::sync::Mutex::new(None),
        session: std::sync::Mutex::new(session),
        ort_profile_path,
        ort_profile_ended: std::sync::atomic::AtomicBool::new(false),
    }))
}

#[cfg(feature = "image-index-sscd")]
fn static_tensor_shape(value_type: &ort::value::ValueType) -> Option<Vec<usize>> {
    let ort::value::ValueType::Tensor { shape, .. } = value_type else {
        return None;
    };
    shape
        .iter()
        .copied()
        .map(usize::try_from)
        .collect::<Result<Vec<_>, _>>()
        .ok()
        .filter(|shape| !shape.is_empty() && shape.iter().all(|dim| *dim > 0))
}

#[cfg(feature = "image-index-sscd")]
fn sscd_ort_intra_threads() -> anyhow::Result<Option<usize>> {
    let Some(value) = env::var(SSCD_ORT_INTRA_THREADS_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let threads = value.parse::<usize>().map_err(|err| {
        anyhow::anyhow!("{SSCD_ORT_INTRA_THREADS_ENV}={value}; expected positive integer: {err}")
    })?;
    if threads == 0 {
        anyhow::bail!("{SSCD_ORT_INTRA_THREADS_ENV}=0; expected positive integer");
    }
    Ok(Some(threads))
}

#[cfg(not(feature = "image-index-sscd"))]
fn load_sscd_runtime(_options: &BuildOptions) -> anyhow::Result<Option<SscdRuntime>> {
    Ok(None)
}

#[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
fn onnx_execution_providers(
    options: &BuildOptions,
) -> anyhow::Result<Vec<ort::ep::ExecutionProviderDispatch>> {
    let mut providers = Vec::new();
    match options.execution_provider.to_ascii_lowercase().as_str() {
        "cuda" => require_cuda_provider(&mut providers)?,
        "tensorrt" | "trt" => {
            require_tensorrt_provider(&mut providers)?;
            push_cuda_provider(&mut providers)?;
        }
        "directml" => require_directml_provider(&mut providers)?,
        "auto" => {
            push_directml_provider(&mut providers);
            push_cuda_provider(&mut providers)?;
        }
        "cpu" | "" => {}
        other => {
            anyhow::bail!(
                "unknown ONNX execution provider: {other}; expected auto, cpu, cuda, tensorrt, or directml"
            );
        }
    }
    Ok(providers)
}

#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
fn require_tensorrt_provider(
    out: &mut Vec<ort::ep::ExecutionProviderDispatch>,
) -> anyhow::Result<()> {
    out.push(configured_tensorrt_provider()?.build().error_on_failure());
    Ok(())
}

#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
fn configured_tensorrt_provider() -> anyhow::Result<ort::ep::TensorRT> {
    let mut provider = ort::ep::TensorRT::default();
    if let Some(value) = bool_env(TENSORRT_FP16_ENV)? {
        provider = provider.with_fp16(value);
    }
    if let Some(value) = bool_env(TENSORRT_INT8_ENV)? {
        provider = provider.with_int8(value);
    }
    if let Some(value) = bool_env(TENSORRT_ENGINE_CACHE_ENV)? {
        provider = provider.with_engine_cache(value);
    }
    if let Ok(path) = env::var(TENSORRT_ENGINE_CACHE_PATH_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            provider = provider
                .with_engine_cache(true)
                .with_engine_cache_path(path);
        }
    }
    if let Some(value) = usize_env(TENSORRT_MAX_WORKSPACE_ENV)? {
        provider = provider.with_max_workspace_size(value);
    }
    if let Some(value) = usize_env(TENSORRT_MIN_SUBGRAPH_ENV)? {
        provider = provider.with_min_subgraph_size(value);
    }
    if let Some(value) = u8_env(TENSORRT_BUILDER_OPT_LEVEL_ENV)? {
        provider = provider.with_builder_optimization_level(value);
    }
    if let Some(value) = bool_env(TENSORRT_CUDA_GRAPH_ENV)? {
        provider = provider.with_cuda_graph(value);
    }
    if let Some(value) = bool_env(TENSORRT_DUMP_SUBGRAPHS_ENV)? {
        provider = provider.with_dump_subgraphs(value);
    }
    if let Some(value) = bool_env(TENSORRT_DETAILED_BUILD_LOG_ENV)? {
        provider = provider.with_detailed_build_log(value);
    }
    if let Ok(path) = env::var(TENSORRT_ONNX_MODEL_FOLDER_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            provider = provider.with_onnx_model_folder_path(path);
        }
    }
    if let Ok(value) = env::var(TENSORRT_PROFILE_MIN_SHAPES_ENV) {
        let value = value.trim();
        if !value.is_empty() {
            provider = provider.with_profile_min_shapes(value);
        }
    }
    if let Ok(value) = env::var(TENSORRT_PROFILE_OPT_SHAPES_ENV) {
        let value = value.trim();
        if !value.is_empty() {
            provider = provider.with_profile_opt_shapes(value);
        }
    }
    if let Ok(value) = env::var(TENSORRT_PROFILE_MAX_SHAPES_ENV) {
        let value = value.trim();
        if !value.is_empty() {
            provider = provider.with_profile_max_shapes(value);
        }
    }
    Ok(provider)
}

#[cfg(all(
    any(feature = "image-index-clip", feature = "image-index-sscd"),
    not(any(
        feature = "image-index-clip-tensorrt",
        feature = "image-index-sscd-tensorrt"
    ))
))]
fn require_tensorrt_provider(
    _out: &mut Vec<ort::ep::ExecutionProviderDispatch>,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "TensorRT execution provider was requested, but this binary was built without an image-index TensorRT EP feature"
    )
}

#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
fn push_cuda_provider(out: &mut Vec<ort::ep::ExecutionProviderDispatch>) -> anyhow::Result<()> {
    out.push(configured_cuda_provider()?.build());
    Ok(())
}

#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
fn require_cuda_provider(out: &mut Vec<ort::ep::ExecutionProviderDispatch>) -> anyhow::Result<()> {
    out.push(configured_cuda_provider()?.build().error_on_failure());
    Ok(())
}

#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
fn configured_cuda_provider() -> anyhow::Result<ort::ep::CUDA> {
    let mut provider = ort::ep::CUDA::default();
    if let Some(value) = bool_env(CUDA_TF32_ENV)? {
        provider = provider.with_tf32(value);
    }
    provider = provider.with_prefer_nhwc(bool_env(CUDA_PREFER_NHWC_ENV)?.unwrap_or(true));
    if let Some(value) = bool_env(CUDA_FUSE_CONV_BIAS_ENV)? {
        provider = provider.with_fuse_conv_bias(value);
    }
    if let Some(value) = bool_env(CUDA_MAX_WORKSPACE_ENV)? {
        provider = provider.with_conv_max_workspace(value);
    }
    if let Some(value) = bool_env_alias(CUDA_GRAPH_ENV, CUDA_GRAPH_LEGACY_ENV)? {
        provider = provider.with_cuda_graph(value);
    }
    if let Some(value) = env::var(CUDA_CONV_ALGO_ENV)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
    {
        let search = match value.as_str() {
            "exhaustive" => ort::ep::cuda::ConvAlgorithmSearch::Exhaustive,
            "heuristic" => ort::ep::cuda::ConvAlgorithmSearch::Heuristic,
            "default" => ort::ep::cuda::ConvAlgorithmSearch::Default,
            other => {
                anyhow::bail!(
                    "{CUDA_CONV_ALGO_ENV}={other}; expected exhaustive, heuristic, or default"
                );
            }
        };
        provider = provider.with_conv_algorithm_search(search);
    }
    Ok(provider)
}

fn bool_env(name: &str) -> anyhow::Result<Option<bool>> {
    let Some(value) = env::var(name)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    match value.as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        other => anyhow::bail!("{name}={other}; expected 1/0, true/false, yes/no, or on/off"),
    }
}

#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
fn bool_env_alias(primary: &str, legacy: &str) -> anyhow::Result<Option<bool>> {
    match bool_env(primary)? {
        Some(value) => Ok(Some(value)),
        None => bool_env(legacy),
    }
}

#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
fn usize_env(name: &str) -> anyhow::Result<Option<usize>> {
    let Some(value) = env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    value
        .parse::<usize>()
        .map(Some)
        .map_err(|err| anyhow::anyhow!("{name}={value}; expected positive integer: {err}"))
}

#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
fn u8_env(name: &str) -> anyhow::Result<Option<u8>> {
    let Some(value) = env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    value
        .parse::<u8>()
        .map(Some)
        .map_err(|err| anyhow::anyhow!("{name}={value}; expected 0-255 integer: {err}"))
}

#[cfg(all(
    any(feature = "image-index-clip", feature = "image-index-sscd"),
    not(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))
))]
fn push_cuda_provider(_out: &mut Vec<ort::ep::ExecutionProviderDispatch>) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(all(
    any(feature = "image-index-clip", feature = "image-index-sscd"),
    not(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))
))]
fn require_cuda_provider(_out: &mut Vec<ort::ep::ExecutionProviderDispatch>) -> anyhow::Result<()> {
    anyhow::bail!(
        "CUDA execution provider was requested, but this binary was built without an image-index CUDA EP feature"
    )
}

#[cfg(any(
    feature = "image-index-clip-directml",
    feature = "image-index-sscd-directml"
))]
fn push_directml_provider(out: &mut Vec<ort::ep::ExecutionProviderDispatch>) {
    out.push(ort::ep::DirectML::default().build());
}

#[cfg(any(
    feature = "image-index-clip-directml",
    feature = "image-index-sscd-directml"
))]
fn require_directml_provider(
    out: &mut Vec<ort::ep::ExecutionProviderDispatch>,
) -> anyhow::Result<()> {
    out.push(ort::ep::DirectML::default().build().error_on_failure());
    Ok(())
}

#[cfg(all(
    any(feature = "image-index-clip", feature = "image-index-sscd"),
    not(any(
        feature = "image-index-clip-directml",
        feature = "image-index-sscd-directml"
    ))
))]
fn push_directml_provider(_out: &mut Vec<ort::ep::ExecutionProviderDispatch>) {}

#[cfg(all(
    any(feature = "image-index-clip", feature = "image-index-sscd"),
    not(any(
        feature = "image-index-clip-directml",
        feature = "image-index-sscd-directml"
    ))
))]
fn require_directml_provider(
    _out: &mut Vec<ort::ep::ExecutionProviderDispatch>,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "DirectML execution provider was requested, but this binary was built without an image-index DirectML EP feature"
    )
}

fn onnx_execution_provider_status(options: &BuildOptions) -> String {
    let requested = options.execution_provider.to_ascii_lowercase();
    if !cfg!(any(
        feature = "image-index-clip",
        feature = "image-index-sscd"
    )) {
        return format!("unavailable:onnx_feature_disabled:requested={requested}");
    }
    let status = match requested.as_str() {
        "cpu" | "" => "available:cpu".to_string(),
        "auto" => format!(
            "requested:auto; candidates={}; explicit_gpu_ep_required=false",
            onnx_auto_provider_candidates()
        ),
        "cuda" => {
            if cfg!(any(
                feature = "image-index-clip-cuda",
                feature = "image-index-sscd-cuda"
            )) {
                let options = cuda_provider_options_status();
                if options.is_empty() {
                    "requested:cuda; build_feature=enabled; registration_required=true".to_string()
                } else {
                    format!(
                        "requested:cuda; build_feature=enabled; registration_required=true; options={options}"
                    )
                }
            } else {
                "unavailable:cuda_build_feature_disabled".to_string()
            }
        }
        "tensorrt" | "trt" => {
            if cfg!(any(
                feature = "image-index-clip-tensorrt",
                feature = "image-index-sscd-tensorrt"
            )) {
                let options = tensorrt_provider_options_status();
                let fallback = if cfg!(any(
                    feature = "image-index-clip-cuda",
                    feature = "image-index-sscd-cuda"
                )) {
                    "cuda"
                } else {
                    "cpu"
                };
                if options.is_empty() {
                    format!(
                        "requested:tensorrt; build_feature=enabled; registration_required=true; fallback={fallback}"
                    )
                } else {
                    format!(
                        "requested:tensorrt; build_feature=enabled; registration_required=true; fallback={fallback}; options={options}"
                    )
                }
            } else {
                "unavailable:tensorrt_build_feature_disabled".to_string()
            }
        }
        "directml" => {
            if cfg!(any(
                feature = "image-index-clip-directml",
                feature = "image-index-sscd-directml"
            )) {
                "requested:directml; build_feature=enabled; registration_required=true".to_string()
            } else {
                "unavailable:directml_build_feature_disabled".to_string()
            }
        }
        other => format!("unavailable:unknown_execution_provider:{other}"),
    };
    append_onnx_runtime_options(status, &requested)
}

#[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
fn append_onnx_runtime_options(status: String, requested: &str) -> String {
    let options = onnx_runtime_options_status(requested);
    if options.is_empty() {
        status
    } else {
        format!("{status}; runtime_options={options}")
    }
}

#[cfg(not(any(feature = "image-index-clip", feature = "image-index-sscd")))]
fn append_onnx_runtime_options(status: String, _requested: &str) -> String {
    status
}

#[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
fn onnx_runtime_options_status(requested: &str) -> String {
    let mut options = Vec::new();
    #[cfg(feature = "image-index-clip")]
    {
        if let Ok(value) = env::var(CLIP_ORT_INTRA_THREADS_ENV) {
            let value = value.trim();
            if !value.is_empty() {
                options.push(format!("{CLIP_ORT_INTRA_THREADS_ENV}={value}"));
            }
        } else if !matches!(requested, "cpu" | "") {
            options.push(format!("{CLIP_ORT_INTRA_THREADS_ENV}=default_gpu_ep_1"));
        }
    }
    #[cfg(feature = "image-index-sscd")]
    {
        if let Ok(value) = env::var(SSCD_ORT_INTRA_THREADS_ENV) {
            let value = value.trim();
            if !value.is_empty() {
                options.push(format!("{SSCD_ORT_INTRA_THREADS_ENV}={value}"));
            }
        } else if !matches!(requested, "cpu" | "") {
            options.push(format!("{SSCD_ORT_INTRA_THREADS_ENV}=default_gpu_ep_1"));
        }
        options.push(format!(
            "{SSCD_RESIZE_BACKEND_ENV}={}",
            match sscd_resize_backend() {
                SscdResizeBackend::FastImageResize => "fast_image_resize",
                SscdResizeBackend::ImageCrate => "image",
            }
        ));
        if sscd_io_binding_enabled() {
            options.push(format!("{SSCD_IO_BINDING_ENV}=1"));
        }
    }
    #[cfg(all(feature = "image-index-clip", feature = "image-index-sscd"))]
    {
        if concurrent_gpu_runs_enabled_for_provider(requested) {
            options.push("QQ_ANALYZER_CONCURRENT_GPU_RUNS=1".to_string());
        }
    }
    #[cfg(any(feature = "image-index-clip", feature = "image-index-sscd"))]
    {
        let prefetch_mode = embed_input_prefetch_mode();
        if prefetch_mode.is_enabled() {
            options.push(format!(
                "{PREFETCH_EMBED_INPUTS_ENV}={}",
                prefetch_mode.status_value()
            ));
        }
        if let Ok(value) = env::var(ASYNC_SQLITE_WRITER_ENV) {
            let value = value.trim();
            if !value.is_empty() {
                options.push(format!("{ASYNC_SQLITE_WRITER_ENV}={value}"));
            }
        }
    }
    options.join(",")
}

#[cfg(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda"))]
fn cuda_provider_options_status() -> String {
    let mut options = Vec::new();
    if env::var(CUDA_PREFER_NHWC_ENV)
        .ok()
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
    {
        options.push(format!("{CUDA_PREFER_NHWC_ENV}=default_1"));
    }
    for name in [
        CUDA_CONV_ALGO_ENV,
        CUDA_MAX_WORKSPACE_ENV,
        CUDA_TF32_ENV,
        CUDA_FUSE_CONV_BIAS_ENV,
        CUDA_GRAPH_ENV,
        CUDA_GRAPH_LEGACY_ENV,
    ] {
        if let Ok(value) = env::var(name) {
            let value = value.trim();
            if !value.is_empty() {
                options.push(format!("{name}={value}"));
            }
        }
    }
    options.join(",")
}

#[cfg(not(any(feature = "image-index-clip-cuda", feature = "image-index-sscd-cuda")))]
fn cuda_provider_options_status() -> String {
    String::new()
}

#[cfg(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
))]
fn tensorrt_provider_options_status() -> String {
    let mut options = Vec::new();
    for name in [
        TENSORRT_FP16_ENV,
        TENSORRT_INT8_ENV,
        TENSORRT_ENGINE_CACHE_ENV,
        TENSORRT_ENGINE_CACHE_PATH_ENV,
        TENSORRT_MAX_WORKSPACE_ENV,
        TENSORRT_MIN_SUBGRAPH_ENV,
        TENSORRT_BUILDER_OPT_LEVEL_ENV,
        TENSORRT_CUDA_GRAPH_ENV,
        TENSORRT_DUMP_SUBGRAPHS_ENV,
        TENSORRT_DETAILED_BUILD_LOG_ENV,
        TENSORRT_ONNX_MODEL_FOLDER_ENV,
        TENSORRT_PROFILE_MIN_SHAPES_ENV,
        TENSORRT_PROFILE_OPT_SHAPES_ENV,
        TENSORRT_PROFILE_MAX_SHAPES_ENV,
    ] {
        if let Ok(value) = env::var(name) {
            let value = value.trim();
            if !value.is_empty() {
                options.push(format!("{name}={value}"));
            }
        }
    }
    options.join(",")
}

#[cfg(not(any(
    feature = "image-index-clip-tensorrt",
    feature = "image-index-sscd-tensorrt"
)))]
fn tensorrt_provider_options_status() -> String {
    String::new()
}

fn onnx_auto_provider_candidates() -> String {
    let mut candidates = Vec::new();
    if cfg!(any(
        feature = "image-index-clip-directml",
        feature = "image-index-sscd-directml"
    )) {
        candidates.push("directml");
    }
    if cfg!(any(
        feature = "image-index-clip-cuda",
        feature = "image-index-sscd-cuda"
    )) {
        candidates.push("cuda");
    }
    candidates.push("cpu");
    candidates.join(",")
}

struct ClipBatchVectors {
    vectors: Vec<Vec<f32>>,
    images: usize,
    preprocess_ms: f64,
    preprocess_resize_cpu_ms: f64,
    preprocess_normalize_cpu_ms: f64,
    tensor_ms: f64,
    run_ms: f64,
    extract_ms: f64,
    total_ms: f64,
}

fn record_clip_batch_profile(profile: Option<&ClipBatchVectors>, profiler: &mut EmbeddingProfiler) {
    if let Some(profile) = profile {
        profiler.report.clip_batches += 1;
        profiler.report.clip_images += profile.images;
        profiler.report.clip_ms += profile.total_ms;
        profiler.report.clip_preprocess_ms += profile.preprocess_ms;
        profiler.report.clip_preprocess_resize_cpu_ms += profile.preprocess_resize_cpu_ms;
        profiler.report.clip_preprocess_normalize_cpu_ms += profile.preprocess_normalize_cpu_ms;
        profiler.report.clip_tensor_ms += profile.tensor_ms;
        profiler.report.clip_run_ms += profile.run_ms;
        profiler.report.clip_extract_ms += profile.extract_ms;
    }
}

#[cfg(feature = "image-index-clip")]
impl ClipVisionRuntime {
    fn ort_profile_path(&self) -> Option<String> {
        self.embedder.session.ort_profile_path()
    }

    fn finish_ort_profiling(&self) -> anyhow::Result<Option<String>> {
        self.embedder
            .session
            .finish_ort_profiling()
            .map_err(|err| anyhow::anyhow!("end CLIP ORT profiling: {err}"))
    }

    fn embed_images_profiled(&self, images: &[DynamicImage]) -> anyhow::Result<ClipBatchVectors> {
        let profile = self
            .embedder
            .embed_images_profiled(images)
            .map_err(|err| anyhow::anyhow!("embed image batch size {}: {err}", images.len()))?;
        Ok(ClipBatchVectors {
            vectors: profile
                .embeddings
                .outer_iter()
                .map(|row| row.to_vec())
                .collect(),
            images: images.len(),
            preprocess_ms: profile.preprocess_ms,
            preprocess_resize_cpu_ms: profile.preprocess_resize_cpu_ms,
            preprocess_normalize_cpu_ms: profile.preprocess_normalize_cpu_ms,
            tensor_ms: profile.tensor_ms,
            run_ms: profile.run_ms,
            extract_ms: profile.extract_ms,
            total_ms: profile.total_ms,
        })
    }

    fn preprocess_images_profiled(
        &self,
        images: &[DynamicImage],
    ) -> anyhow::Result<open_clip_inference::vision::VisionPreprocessedBatch> {
        self.embedder
            .preprocess_batch_profiled(images)
            .map_err(|err| anyhow::anyhow!("preprocess CLIP batch size {}: {err}", images.len()))
    }

    fn embed_preprocessed_images_profiled(
        &self,
        input: open_clip_inference::vision::VisionPreprocessedBatch,
    ) -> anyhow::Result<ClipBatchVectors> {
        let profile = self
            .embedder
            .embed_preprocessed_batch_profiled(input)
            .map_err(|err| anyhow::anyhow!("embed preprocessed CLIP batch: {err}"))?;
        Ok(ClipBatchVectors {
            vectors: profile
                .embeddings
                .outer_iter()
                .map(|row| row.to_vec())
                .collect(),
            images: profile.embeddings.shape().first().copied().unwrap_or(0),
            preprocess_ms: profile.preprocess_ms,
            preprocess_resize_cpu_ms: profile.preprocess_resize_cpu_ms,
            preprocess_normalize_cpu_ms: profile.preprocess_normalize_cpu_ms,
            tensor_ms: profile.tensor_ms,
            run_ms: profile.run_ms,
            extract_ms: profile.extract_ms,
            total_ms: profile.total_ms,
        })
    }

    fn embed_image(&self, image: &DynamicImage) -> anyhow::Result<Vec<f32>> {
        let embedding = self
            .embedder
            .embed_image(image)
            .map_err(|err| anyhow::anyhow!("embed image: {err}"))?;
        Ok(embedding.to_vec())
    }

    fn embed_image_path(&self, path: &Path) -> anyhow::Result<Vec<f32>> {
        let image = image::ImageReader::open(fs_path_for_host(path).as_ref())?
            .with_guessed_format()?
            .decode()?;
        self.embed_image(&image)
            .map_err(|err| anyhow::anyhow!("embed image {}: {err}", path.display()))
    }
}

#[cfg(not(feature = "image-index-clip"))]
impl ClipVisionRuntime {
    fn ort_profile_path(&self) -> Option<String> {
        None
    }

    fn finish_ort_profiling(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    fn embed_images_profiled(&self, _images: &[DynamicImage]) -> anyhow::Result<ClipBatchVectors> {
        anyhow::bail!("image-index-clip feature is not enabled")
    }

    fn embed_image(&self, _image: &DynamicImage) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("image-index-clip feature is not enabled")
    }

    fn embed_image_path(&self, _path: &Path) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("image-index-clip feature is not enabled")
    }
}

#[cfg(feature = "image-index-sscd")]
impl SscdRuntime {
    fn ort_profile_path(&self) -> Option<String> {
        self.ort_profile_path
            .as_ref()
            .map(|path| path.display().to_string())
    }

    fn finish_ort_profiling(&self) -> anyhow::Result<Option<String>> {
        if self.ort_profile_path.is_none()
            || self
                .ort_profile_ended
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Ok(None);
        }
        let mut session = self
            .session
            .lock()
            .map_err(|err| anyhow::anyhow!("SSCD session lock poisoned: {err}"))?;
        let path = session
            .end_profiling()
            .map_err(|err| anyhow::anyhow!("end SSCD ORT profiling: {err}"))?;
        Ok(Some(path))
    }

    fn embed_images_profiled(
        &self,
        images: &[DynamicImage],
        profiler: &mut EmbeddingProfiler,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let tensor_batch_size = self.tensor_batch_size_for(images.len())?;
        let (input, preprocess_profile) =
            sscd_preprocess_batch_timed(images, self.input_tensor_type, tensor_batch_size)?;
        self.embed_preprocessed_images_profiled(input, images.len(), preprocess_profile, profiler)
    }

    fn tensor_batch_size_for(&self, actual_batch_size: usize) -> anyhow::Result<usize> {
        let Some(static_batch_size) = self.static_input_batch_size else {
            return Ok(actual_batch_size);
        };
        if actual_batch_size > static_batch_size {
            anyhow::bail!(
                "SSCD batch size {actual_batch_size} exceeds static model batch {static_batch_size}"
            );
        }
        Ok(static_batch_size)
    }

    fn can_use_io_binding(&self) -> bool {
        sscd_io_binding_enabled()
            && self.static_input_batch_size.is_some()
            && self.static_output_shape.is_some()
            && self.output_tensor_type == ort::value::TensorElementType::Float32
    }

    fn embed_preprocessed_images_profiled(
        &self,
        input: SscdPreprocessedBatch,
        expected_batch: usize,
        preprocess_profile: SscdPreprocessProfile,
        profiler: &mut EmbeddingProfiler,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let total_started = Instant::now();
        profiler.report.sscd_batches += 1;
        profiler.report.sscd_images += expected_batch;
        profiler.report.sscd_preprocess_workers = profiler
            .report
            .sscd_preprocess_workers
            .max(preprocess_profile.workers);
        profiler.report.sscd_preprocess_ms += preprocess_profile.wall_ms;
        profiler.report.sscd_input_resize_cpu_ms += preprocess_profile.resize_cpu_ms;
        profiler.report.sscd_input_normalize_cpu_ms += preprocess_profile.normalize_cpu_ms;
        if self.can_use_io_binding() {
            let vectors =
                self.embed_preprocessed_images_iobinding(input, expected_batch, profiler)?;
            profiler.report.sscd_total_ms +=
                preprocess_profile.wall_ms + elapsed_ms_f64(total_started);
            return Ok(vectors);
        }
        let started = Instant::now();
        let input_tensor = match input {
            SscdPreprocessedBatch::F32(input) => ort::value::Value::from_array(input)?.into_dyn(),
            SscdPreprocessedBatch::F16(input) => ort::value::Value::from_array(input)?.into_dyn(),
        };
        EmbeddingProfiler::add_ms(&mut profiler.report.sscd_tensor_ms, started);
        let mut session = self
            .session
            .lock()
            .map_err(|err| anyhow::anyhow!("SSCD session lock poisoned: {err}"))?;
        let started = Instant::now();
        let outputs = session.run(ort::inputs![self.input_name.as_str() => input_tensor])?;
        EmbeddingProfiler::add_ms(&mut profiler.report.sscd_run_ms, started);
        let started = Instant::now();
        let (batch, dim, data) =
            extract_sscd_output(&outputs[0], self.output_tensor_type, expected_batch)?;
        EmbeddingProfiler::add_ms(&mut profiler.report.sscd_extract_ms, started);
        let started = Instant::now();
        let vectors = normalize_sscd_vectors(batch, dim, &data)?;
        EmbeddingProfiler::add_ms(&mut profiler.report.sscd_normalize_ms, started);
        profiler.report.sscd_total_ms += preprocess_profile.wall_ms + elapsed_ms_f64(total_started);
        Ok(vectors)
    }

    fn embed_preprocessed_images_iobinding(
        &self,
        input: SscdPreprocessedBatch,
        expected_batch: usize,
        profiler: &mut EmbeddingProfiler,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let output_shape = self
            .static_output_shape
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("SSCD output shape is dynamic"))?
            .clone();
        let started = Instant::now();
        let (batch, dim, data) = {
            let mut session = self
                .session
                .lock()
                .map_err(|err| anyhow::anyhow!("SSCD session lock poisoned: {err}"))?;
            let mut binding_guard = self
                .io_binding
                .lock()
                .map_err(|err| anyhow::anyhow!("SSCD IO binding lock poisoned: {err}"))?;
            match input {
                SscdPreprocessedBatch::F32(input) => {
                    let input_shape = input.shape().to_vec();
                    let input_data = input
                        .as_slice()
                        .ok_or_else(|| anyhow::anyhow!("SSCD input tensor is not contiguous"))?;
                    let binding = ensure_sscd_iobinding_f32(
                        &mut binding_guard,
                        &session,
                        &self.output_name,
                        &input_shape,
                        &output_shape,
                    )?;
                    let (_, pinned_input) = binding.input_cpu.extract_tensor_mut();
                    pinned_input.copy_from_slice(input_data);
                    binding.input_cpu.copy_into(&mut binding.input_device)?;
                    binding
                        .binding
                        .bind_input(&self.input_name, &binding.input_device)?;
                    EmbeddingProfiler::add_ms(&mut profiler.report.sscd_tensor_ms, started);
                    let run_started = Instant::now();
                    let outputs = session.run_binding(&binding.binding)?;
                    EmbeddingProfiler::add_ms(&mut profiler.report.sscd_run_ms, run_started);
                    let extract_started = Instant::now();
                    let output = outputs.get(&self.output_name).ok_or_else(|| {
                        anyhow::anyhow!(
                            "SSCD output {} missing from IO binding run",
                            self.output_name
                        )
                    })?;
                    let output = output.downcast_ref::<ort::value::TensorValueType<f32>>()?;
                    output.copy_into(&mut binding.output_cpu)?;
                    let (shape, output_data) = binding.output_cpu.try_extract_tensor::<f32>()?;
                    let (batch, dim) =
                        validate_sscd_output_shape(shape, output_data.len(), expected_batch)?;
                    let data = output_data[..expected_batch * dim].to_vec();
                    EmbeddingProfiler::add_ms(
                        &mut profiler.report.sscd_extract_ms,
                        extract_started,
                    );
                    (batch, dim, data)
                }
                SscdPreprocessedBatch::F16(input) => {
                    let input_shape = input.shape().to_vec();
                    let input_data = input
                        .as_slice()
                        .ok_or_else(|| anyhow::anyhow!("SSCD input tensor is not contiguous"))?;
                    let binding = ensure_sscd_iobinding_f16(
                        &mut binding_guard,
                        &session,
                        &self.output_name,
                        &input_shape,
                        &output_shape,
                    )?;
                    let (_, pinned_input) = binding.input_cpu.extract_tensor_mut();
                    pinned_input.copy_from_slice(input_data);
                    binding.input_cpu.copy_into(&mut binding.input_device)?;
                    binding
                        .binding
                        .bind_input(&self.input_name, &binding.input_device)?;
                    EmbeddingProfiler::add_ms(&mut profiler.report.sscd_tensor_ms, started);
                    let run_started = Instant::now();
                    let outputs = session.run_binding(&binding.binding)?;
                    EmbeddingProfiler::add_ms(&mut profiler.report.sscd_run_ms, run_started);
                    let extract_started = Instant::now();
                    let output = outputs.get(&self.output_name).ok_or_else(|| {
                        anyhow::anyhow!(
                            "SSCD output {} missing from IO binding run",
                            self.output_name
                        )
                    })?;
                    let output = output.downcast_ref::<ort::value::TensorValueType<f32>>()?;
                    output.copy_into(&mut binding.output_cpu)?;
                    let (shape, output_data) = binding.output_cpu.try_extract_tensor::<f32>()?;
                    let (batch, dim) =
                        validate_sscd_output_shape(shape, output_data.len(), expected_batch)?;
                    let data = output_data[..expected_batch * dim].to_vec();
                    EmbeddingProfiler::add_ms(
                        &mut profiler.report.sscd_extract_ms,
                        extract_started,
                    );
                    (batch, dim, data)
                }
            }
        };
        let started = Instant::now();
        let vectors = normalize_sscd_vectors(batch, dim, &data)?;
        EmbeddingProfiler::add_ms(&mut profiler.report.sscd_normalize_ms, started);
        Ok(vectors)
    }

    fn embed_image(&self, image: &DynamicImage) -> anyhow::Result<Vec<f32>> {
        let mut profiler = EmbeddingProfiler::default();
        let vectors = self.embed_images_profiled(std::slice::from_ref(image), &mut profiler)?;
        vectors
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("SSCD returned no embedding"))
    }

    fn embed_image_path(&self, path: &Path) -> anyhow::Result<Vec<f32>> {
        let image = image::ImageReader::open(fs_path_for_host(path).as_ref())?
            .with_guessed_format()?
            .decode()?;
        self.embed_image(&image)
            .map_err(|err| anyhow::anyhow!("embed SSCD image {}: {err}", path.display()))
    }
}

#[cfg(feature = "image-index-sscd")]
fn sscd_io_binding_enabled() -> bool {
    env::var(SSCD_IO_BINDING_ENV)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(feature = "image-index-sscd")]
fn ensure_sscd_iobinding_f32<'a>(
    state: &'a mut Option<SscdIoBinding>,
    session: &ort::session::Session,
    output_name: &str,
    input_shape: &[usize],
    output_shape: &[usize],
) -> anyhow::Result<&'a mut SscdIoBindingF32> {
    let recreate = state
        .as_ref()
        .map(|state| {
            !matches!(state.state, SscdIoBindingState::F32(_))
                || state.input_shape != input_shape
                || state.output_shape != output_shape
        })
        .unwrap_or(true);
    if recreate {
        use ort::memory::{AllocationDevice, Allocator, AllocatorType, MemoryInfo, MemoryType};
        let input_cpu_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA_PINNED,
                0,
                AllocatorType::Device,
                MemoryType::CPUInput,
            )?,
        )?;
        let input_cpu = ort::value::Tensor::<f32>::new(&input_cpu_allocator, input_shape.to_vec())?;
        let device_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA,
                0,
                AllocatorType::Device,
                MemoryType::Default,
            )?,
        )?;
        let input_device = ort::value::Tensor::<f32>::new(&device_allocator, input_shape.to_vec())?;
        let output_device =
            ort::value::Tensor::<f32>::new(&device_allocator, output_shape.to_vec())?;
        let output_cpu_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA_PINNED,
                0,
                AllocatorType::Device,
                MemoryType::CPUOutput,
            )?,
        )?;
        let output_cpu =
            ort::value::Tensor::<f32>::new(&output_cpu_allocator, output_shape.to_vec())?;
        let mut binding = session.create_binding()?;
        binding.bind_output(output_name, output_device)?;
        *state = Some(SscdIoBinding {
            input_shape: input_shape.to_vec(),
            output_shape: output_shape.to_vec(),
            state: SscdIoBindingState::F32(SscdIoBindingF32 {
                input_cpu,
                input_device,
                output_cpu,
                binding,
            }),
        });
    }
    match state.as_mut() {
        Some(SscdIoBinding {
            state: SscdIoBindingState::F32(state),
            ..
        }) => Ok(state),
        _ => anyhow::bail!("failed to initialize SSCD f32 IO binding"),
    }
}

#[cfg(feature = "image-index-sscd")]
fn ensure_sscd_iobinding_f16<'a>(
    state: &'a mut Option<SscdIoBinding>,
    session: &ort::session::Session,
    output_name: &str,
    input_shape: &[usize],
    output_shape: &[usize],
) -> anyhow::Result<&'a mut SscdIoBindingF16> {
    let recreate = state
        .as_ref()
        .map(|state| {
            !matches!(state.state, SscdIoBindingState::F16(_))
                || state.input_shape != input_shape
                || state.output_shape != output_shape
        })
        .unwrap_or(true);
    if recreate {
        use ort::memory::{AllocationDevice, Allocator, AllocatorType, MemoryInfo, MemoryType};
        let input_cpu_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA_PINNED,
                0,
                AllocatorType::Device,
                MemoryType::CPUInput,
            )?,
        )?;
        let input_cpu =
            ort::value::Tensor::<half::f16>::new(&input_cpu_allocator, input_shape.to_vec())?;
        let device_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA,
                0,
                AllocatorType::Device,
                MemoryType::Default,
            )?,
        )?;
        let input_device =
            ort::value::Tensor::<half::f16>::new(&device_allocator, input_shape.to_vec())?;
        let output_device =
            ort::value::Tensor::<f32>::new(&device_allocator, output_shape.to_vec())?;
        let output_cpu_allocator = Allocator::new(
            session,
            MemoryInfo::new(
                AllocationDevice::CUDA_PINNED,
                0,
                AllocatorType::Device,
                MemoryType::CPUOutput,
            )?,
        )?;
        let output_cpu =
            ort::value::Tensor::<f32>::new(&output_cpu_allocator, output_shape.to_vec())?;
        let mut binding = session.create_binding()?;
        binding.bind_output(output_name, output_device)?;
        *state = Some(SscdIoBinding {
            input_shape: input_shape.to_vec(),
            output_shape: output_shape.to_vec(),
            state: SscdIoBindingState::F16(SscdIoBindingF16 {
                input_cpu,
                input_device,
                output_cpu,
                binding,
            }),
        });
    }
    match state.as_mut() {
        Some(SscdIoBinding {
            state: SscdIoBindingState::F16(state),
            ..
        }) => Ok(state),
        _ => anyhow::bail!("failed to initialize SSCD f16 IO binding"),
    }
}

#[cfg(feature = "image-index-sscd")]
fn normalize_sscd_vectors(batch: usize, dim: usize, data: &[f32]) -> anyhow::Result<Vec<Vec<f32>>> {
    let mut vectors = Vec::with_capacity(batch);
    for (index, row) in data.chunks_exact(dim).take(batch).enumerate() {
        if !row.iter().all(|value| value.is_finite()) {
            anyhow::bail!("SSCD embedding {index} contains non-finite values");
        }
        let mut vector = row.to_vec();
        l2_normalize_in_place(&mut vector)?;
        vectors.push(vector);
    }
    Ok(vectors)
}

#[cfg(feature = "image-index-sscd")]
fn extract_sscd_output(
    output: &ort::value::DynValue,
    output_type: ort::value::TensorElementType,
    expected_batch: usize,
) -> anyhow::Result<(usize, usize, Vec<f32>)> {
    match output_type {
        ort::value::TensorElementType::Float32 => {
            let (shape, data) = output.try_extract_tensor::<f32>()?;
            let (batch, dim) = validate_sscd_output_shape(shape, data.len(), expected_batch)?;
            Ok((batch, dim, data[..batch * dim].to_vec()))
        }
        ort::value::TensorElementType::Float16 => {
            let (shape, data) = output.try_extract_tensor::<half::f16>()?;
            let (batch, dim) = validate_sscd_output_shape(shape, data.len(), expected_batch)?;
            Ok((
                batch,
                dim,
                data[..batch * dim]
                    .iter()
                    .map(|value| value.to_f32())
                    .collect(),
            ))
        }
        other => anyhow::bail!("unsupported SSCD output tensor type: {other}"),
    }
}

#[cfg(feature = "image-index-sscd")]
fn validate_sscd_output_shape(
    shape: &ort::value::Shape,
    data_len: usize,
    expected_batch: usize,
) -> anyhow::Result<(usize, usize)> {
    if shape.len() != 2 {
        anyhow::bail!("SSCD output shape is {:?}, expected [batch, dim]", shape);
    }
    let batch = shape[0] as usize;
    let dim = shape[1] as usize;
    if batch < expected_batch || data_len != batch * dim {
        anyhow::bail!(
            "SSCD output shape {:?} is too small for {} input images",
            shape,
            expected_batch
        );
    }
    Ok((expected_batch, dim))
}

#[cfg(feature = "image-index-sscd")]
impl Drop for SscdRuntime {
    fn drop(&mut self) {
        let _ = self.finish_ort_profiling();
        if let Ok(state) = self.io_binding.get_mut() {
            // ORT CUDA may retain stream-owned values in the binding until process teardown.
            // Dropping them before the session has caused native access violations.
            if let Some(binding) = state.take() {
                std::mem::forget(binding);
            }
        }
    }
}

#[cfg(not(feature = "image-index-sscd"))]
impl SscdRuntime {
    fn ort_profile_path(&self) -> Option<String> {
        None
    }

    fn finish_ort_profiling(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    fn embed_images_profiled(
        &self,
        _images: &[DynamicImage],
        _profiler: &mut EmbeddingProfiler,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("image-index-sscd feature is not enabled")
    }

    fn embed_image(&self, _image: &DynamicImage) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("image-index-sscd feature is not enabled")
    }

    fn embed_image_path(&self, _path: &Path) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("image-index-sscd feature is not enabled")
    }
}

#[cfg(feature = "image-index-sscd")]
fn sscd_preprocess_batch_timed(
    images: &[DynamicImage],
    input_tensor_type: ort::value::TensorElementType,
    tensor_batch_size: usize,
) -> anyhow::Result<(SscdPreprocessedBatch, SscdPreprocessProfile)> {
    if tensor_batch_size < images.len() {
        anyhow::bail!(
            "SSCD tensor batch {tensor_batch_size} is smaller than {} images",
            images.len()
        );
    }
    let started = Instant::now();
    let workers = sscd_preprocess_workers(images.len());
    let (input, profile) = match input_tensor_type {
        ort::value::TensorElementType::Float32 => {
            let (input, profile) =
                sscd_preprocess_batch_with_workers(images, workers, tensor_batch_size)?;
            (SscdPreprocessedBatch::F32(input), profile)
        }
        ort::value::TensorElementType::Float16 => {
            let (input, profile) =
                sscd_preprocess_batch_with_workers_f16(images, workers, tensor_batch_size)?;
            (SscdPreprocessedBatch::F16(input), profile)
        }
        other => anyhow::bail!("unsupported SSCD input tensor type: {other}"),
    };
    Ok((
        input,
        profile.with_wall_and_workers(elapsed_ms_f64(started), workers),
    ))
}

#[cfg(feature = "image-index-sscd")]
fn sscd_preprocess_batch_with_workers(
    images: &[DynamicImage],
    workers: usize,
    tensor_batch_size: usize,
) -> anyhow::Result<(ndarray::Array4<f32>, SscdPreprocessProfile)> {
    if images.is_empty() {
        anyhow::bail!("empty SSCD batch");
    }
    let size = SSCD_INPUT_SIZE as usize;
    let image_stride = 3usize
        .checked_mul(size)
        .and_then(|value| value.checked_mul(size))
        .ok_or_else(|| anyhow::anyhow!("SSCD input dimensions overflow"))?;
    let mut data = vec![0.0f32; tensor_batch_size.saturating_mul(image_stride)];
    let mut profile = SscdPreprocessProfile::default();
    if workers == 1 {
        for (image, output) in images.iter().zip(data.chunks_exact_mut(image_stride)) {
            profile = profile.add_cpu(sscd_preprocess_image_profiled(image, output, size)?);
        }
    } else {
        let chunk_images = images.len().div_ceil(workers);
        std::thread::scope(|scope| -> anyhow::Result<()> {
            let mut handles = Vec::new();
            let actual_data = &mut data[..images.len() * image_stride];
            for (chunk_idx, output) in actual_data
                .chunks_mut(chunk_images * image_stride)
                .enumerate()
            {
                let start = chunk_idx * chunk_images;
                let count = output.len() / image_stride;
                let image_chunk = &images[start..start + count];
                handles.push(
                    scope.spawn(move || -> anyhow::Result<SscdPreprocessProfile> {
                        let mut profile = SscdPreprocessProfile::default();
                        for (image, output) in image_chunk
                            .iter()
                            .zip(output.chunks_exact_mut(image_stride))
                        {
                            profile = profile
                                .add_cpu(sscd_preprocess_image_profiled(image, output, size)?);
                        }
                        Ok(profile)
                    }),
                );
            }
            for handle in handles {
                profile = profile.add_cpu(
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("SSCD preprocess worker panicked"))??,
                );
            }
            Ok(())
        })?;
    }
    let input = ndarray::Array4::from_shape_vec((tensor_batch_size, 3, size, size), data)
        .map_err(|err| anyhow::anyhow!("build SSCD input tensor: {err}"))?;
    Ok((input, profile))
}

#[cfg(feature = "image-index-sscd")]
fn sscd_preprocess_batch_with_workers_f16(
    images: &[DynamicImage],
    workers: usize,
    tensor_batch_size: usize,
) -> anyhow::Result<(ndarray::Array4<half::f16>, SscdPreprocessProfile)> {
    if images.is_empty() {
        anyhow::bail!("empty SSCD batch");
    }
    let size = SSCD_INPUT_SIZE as usize;
    let image_stride = 3usize
        .checked_mul(size)
        .and_then(|value| value.checked_mul(size))
        .ok_or_else(|| anyhow::anyhow!("SSCD input dimensions overflow"))?;
    let mut data = vec![half::f16::ZERO; tensor_batch_size.saturating_mul(image_stride)];
    let mut profile = SscdPreprocessProfile::default();
    if workers == 1 {
        for (image, output) in images.iter().zip(data.chunks_exact_mut(image_stride)) {
            profile = profile.add_cpu(sscd_preprocess_image_profiled_f16(image, output, size)?);
        }
    } else {
        let chunk_images = images.len().div_ceil(workers);
        std::thread::scope(|scope| -> anyhow::Result<()> {
            let mut handles = Vec::new();
            let actual_data = &mut data[..images.len() * image_stride];
            for (chunk_idx, output) in actual_data
                .chunks_mut(chunk_images * image_stride)
                .enumerate()
            {
                let start = chunk_idx * chunk_images;
                let count = output.len() / image_stride;
                let image_chunk = &images[start..start + count];
                handles.push(
                    scope.spawn(move || -> anyhow::Result<SscdPreprocessProfile> {
                        let mut profile = SscdPreprocessProfile::default();
                        for (image, output) in image_chunk
                            .iter()
                            .zip(output.chunks_exact_mut(image_stride))
                        {
                            profile = profile
                                .add_cpu(sscd_preprocess_image_profiled_f16(image, output, size)?);
                        }
                        Ok(profile)
                    }),
                );
            }
            for handle in handles {
                profile = profile.add_cpu(
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("SSCD preprocess worker panicked"))??,
                );
            }
            Ok(())
        })?;
    }
    let input = ndarray::Array4::from_shape_vec((tensor_batch_size, 3, size, size), data)
        .map_err(|err| anyhow::anyhow!("build SSCD f16 input tensor: {err}"))?;
    Ok((input, profile))
}

#[cfg(feature = "image-index-sscd")]
fn sscd_preprocess_workers(image_count: usize) -> usize {
    let requested = env::var("QQ_ANALYZER_SSCD_PREPROCESS_WORKERS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SSCD_PREPROCESS_WORKERS_CAP);
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .min(requested.min(MAX_SSCD_PREPROCESS_WORKERS))
        .max(1)
        .min(image_count.max(1))
}

#[cfg(feature = "image-index-sscd")]
fn sscd_preprocess_image_profiled(
    image: &DynamicImage,
    output: &mut [f32],
    size: usize,
) -> anyhow::Result<SscdPreprocessProfile> {
    let resize_started = Instant::now();
    let pixels = sscd_resize_rgb_pixels(image, size)?;
    let resize_cpu_ms = elapsed_ms_f64(resize_started);
    let normalize_started = Instant::now();
    sscd_normalize_rgb_pixels(&pixels, output, size);
    let normalize_cpu_ms = elapsed_ms_f64(normalize_started);
    Ok(SscdPreprocessProfile {
        resize_cpu_ms,
        normalize_cpu_ms,
        ..SscdPreprocessProfile::default()
    })
}

#[cfg(feature = "image-index-sscd")]
fn sscd_preprocess_image_profiled_f16(
    image: &DynamicImage,
    output: &mut [half::f16],
    size: usize,
) -> anyhow::Result<SscdPreprocessProfile> {
    let resize_started = Instant::now();
    let pixels = sscd_resize_rgb_pixels(image, size)?;
    let resize_cpu_ms = elapsed_ms_f64(resize_started);
    let normalize_started = Instant::now();
    sscd_normalize_rgb_pixels_f16(&pixels, output, size);
    let normalize_cpu_ms = elapsed_ms_f64(normalize_started);
    Ok(SscdPreprocessProfile {
        resize_cpu_ms,
        normalize_cpu_ms,
        ..SscdPreprocessProfile::default()
    })
}

#[cfg(feature = "image-index-sscd")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SscdResizeBackend {
    FastImageResize,
    ImageCrate,
}

#[cfg(feature = "image-index-sscd")]
fn sscd_resize_backend() -> SscdResizeBackend {
    static BACKEND: OnceLock<SscdResizeBackend> = OnceLock::new();
    *BACKEND.get_or_init(|| {
        env::var(SSCD_RESIZE_BACKEND_ENV)
            .ok()
            .map(|value| match value.trim().to_ascii_lowercase().as_str() {
                "fast" | "fast-image-resize" | "fast_image_resize" => {
                    SscdResizeBackend::FastImageResize
                }
                _ => SscdResizeBackend::ImageCrate,
            })
            .unwrap_or_else(|| {
                if cfg!(feature = "fast_image_resize") {
                    SscdResizeBackend::FastImageResize
                } else {
                    SscdResizeBackend::ImageCrate
                }
            })
    })
}

#[cfg(feature = "image-index-sscd")]
fn sscd_resize_rgb_pixels(image: &DynamicImage, size: usize) -> anyhow::Result<Vec<u8>> {
    match sscd_resize_backend() {
        SscdResizeBackend::FastImageResize => sscd_resize_rgb_pixels_fast(image, size),
        SscdResizeBackend::ImageCrate => sscd_resize_rgb_pixels_image(image, size),
    }
}

#[cfg(all(feature = "image-index-sscd", feature = "fast_image_resize"))]
fn sscd_resize_rgb_pixels_fast(image: &DynamicImage, size: usize) -> anyhow::Result<Vec<u8>> {
    use fast_image_resize::{
        images::{Image, ImageRef},
        FilterType as FirFilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    };

    let size_u32 = u32::try_from(size).map_err(|err| anyhow::anyhow!("SSCD resize size: {err}"))?;
    let (width, height) = image.dimensions();
    let mut dst_image = Image::new(size_u32, size_u32, PixelType::U8x3);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FirFilterType::Bilinear));
    let mut resizer = Resizer::new();
    if let Some(rgb_image) = image.as_rgb8() {
        let src_image = ImageRef::new(width, height, rgb_image.as_raw(), PixelType::U8x3)
            .map_err(|err| anyhow::anyhow!("build borrowed SSCD resize source: {err}"))?;
        resizer
            .resize(&src_image, &mut dst_image, &options)
            .map_err(|err| anyhow::anyhow!("resize SSCD input: {err}"))?;
    } else {
        let rgb_image = image.to_rgb8();
        let src_image = Image::from_vec_u8(width, height, rgb_image.into_raw(), PixelType::U8x3)
            .map_err(|err| anyhow::anyhow!("build SSCD resize source: {err}"))?;
        resizer
            .resize(&src_image, &mut dst_image, &options)
            .map_err(|err| anyhow::anyhow!("resize SSCD input: {err}"))?;
    }
    Ok(dst_image.into_vec())
}

#[cfg(all(feature = "image-index-sscd", not(feature = "fast_image_resize")))]
fn sscd_resize_rgb_pixels_fast(image: &DynamicImage, size: usize) -> anyhow::Result<Vec<u8>> {
    sscd_resize_rgb_pixels_image(image, size)
}

#[cfg(feature = "image-index-sscd")]
fn sscd_resize_rgb_pixels_image(image: &DynamicImage, size: usize) -> anyhow::Result<Vec<u8>> {
    let size_u32 = u32::try_from(size).map_err(|err| anyhow::anyhow!("SSCD resize size: {err}"))?;
    Ok(image
        .resize_exact(size_u32, size_u32, image::imageops::FilterType::Triangle)
        .to_rgb8()
        .into_raw())
}

#[cfg(feature = "image-index-sscd")]
fn sscd_normalize_rgb_pixels(pixels: &[u8], output: &mut [f32], size: usize) {
    let mean = [0.485f32, 0.456, 0.406];
    let std = [0.229f32, 0.224, 0.225];
    let plane = size * size;
    debug_assert_eq!(pixels.len(), plane * 3);
    debug_assert_eq!(output.len(), plane * 3);
    let mut table = [[0.0f32; 256]; 3];
    for channel in 0..3 {
        for value in 0..=u8::MAX {
            table[channel][usize::from(value)] =
                (f32::from(value) / 255.0 - mean[channel]) / std[channel];
        }
    }
    let (red, rest) = output.split_at_mut(plane);
    let (green, blue) = rest.split_at_mut(plane);
    for (offset, pixel) in pixels.chunks_exact(3).enumerate() {
        red[offset] = table[0][usize::from(pixel[0])];
        green[offset] = table[1][usize::from(pixel[1])];
        blue[offset] = table[2][usize::from(pixel[2])];
    }
}

#[cfg(feature = "image-index-sscd")]
fn sscd_normalize_rgb_pixels_f16(pixels: &[u8], output: &mut [half::f16], size: usize) {
    let mean = [0.485f32, 0.456, 0.406];
    let std = [0.229f32, 0.224, 0.225];
    let plane = size * size;
    debug_assert_eq!(pixels.len(), plane * 3);
    debug_assert_eq!(output.len(), plane * 3);
    let mut table = [[half::f16::ZERO; 256]; 3];
    for channel in 0..3 {
        for value in 0..=u8::MAX {
            table[channel][usize::from(value)] =
                half::f16::from_f32((f32::from(value) / 255.0 - mean[channel]) / std[channel]);
        }
    }
    let (red, rest) = output.split_at_mut(plane);
    let (green, blue) = rest.split_at_mut(plane);
    for (offset, pixel) in pixels.chunks_exact(3).enumerate() {
        red[offset] = table[0][usize::from(pixel[0])];
        green[offset] = table[1][usize::from(pixel[1])];
        blue[offset] = table[2][usize::from(pixel[2])];
    }
}

#[cfg(feature = "image-index-sscd")]
fn l2_normalize_in_place(vector: &mut [f32]) -> anyhow::Result<()> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        anyhow::bail!("SSCD embedding has invalid L2 norm {norm}");
    }
    for value in vector {
        *value /= norm;
    }
    Ok(())
}

#[cfg(feature = "image-index-clip")]
impl ClipTextRuntime {
    fn embed_text(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let embedding = self
            .embedder
            .embed_text(text)
            .map_err(|err| anyhow::anyhow!("embed text: {err}"))?;
        Ok(embedding.to_vec())
    }
}

#[cfg(not(feature = "image-index-clip"))]
impl ClipTextRuntime {
    fn embed_text(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("image-index-clip feature is not enabled")
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn feature_unavailable_json(account: &str, action: &str) -> String {
    json!({
        "account": account,
        "action": action,
        "available": false,
        "error": "image-index feature is not enabled in this build"
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_scaling_selects_smallest_factor_that_covers_target() {
        let factors = [
            JpegScalingFactor { num: 1, denom: 1 },
            JpegScalingFactor { num: 7, denom: 8 },
            JpegScalingFactor { num: 3, denom: 4 },
            JpegScalingFactor { num: 1, denom: 2 },
            JpegScalingFactor { num: 3, denom: 8 },
            JpegScalingFactor { num: 1, denom: 4 },
            JpegScalingFactor { num: 1, denom: 8 },
        ];
        assert_eq!(
            select_jpeg_scaling_factor(1920, 1080, 320, &factors),
            JpegScalingFactor { num: 3, denom: 8 }
        );
        assert_eq!(jpeg_scaled_dimension(1081, factors[3]), Some(541));
    }

    #[test]
    fn jpeg_scaling_keeps_original_when_an_edge_is_below_target() {
        let factors = [
            JpegScalingFactor { num: 1, denom: 1 },
            JpegScalingFactor { num: 1, denom: 2 },
        ];
        assert_eq!(
            select_jpeg_scaling_factor(4096, 240, 320, &factors),
            JpegScalingFactor { num: 1, denom: 1 }
        );
        assert_eq!(
            select_jpeg_scaling_factor(640, 640, 0, &factors),
            JpegScalingFactor { num: 1, denom: 1 }
        );
    }

    #[test]
    fn persistent_prepare_pool_preserves_batch_order() {
        let root = unique_temp_root("image-index-persistent-prepare-pool");
        std::fs::create_dir_all(&root).unwrap();
        let mut assets = Vec::new();
        let mut expected_paths = Vec::new();
        for index in 0..8 {
            let path = root.join(format!("{index:02}.png"));
            write_test_png(&path, [index as u8, 64, 192]);
            let meta = std::fs::metadata(&path).unwrap();
            let mtime_unix = meta
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let (fingerprint, _) = fingerprint_image_for_index(&path, false).unwrap();
            expected_paths.push(path.clone());
            assets.push(PendingEmbeddingAsset {
                path,
                source_root: root.clone(),
                file_size: meta.len() as i64,
                mtime_unix,
                fingerprint: Some(fingerprint),
            });
        }

        let pool = EmbeddingPreparePool::start(4).unwrap();
        let handle = pool.submit(assets, None).unwrap().unwrap();
        let batch = join_prepare_batch(handle).unwrap();
        assert_eq!(batch.stats.workers, 4);
        assert_eq!(batch.stats.profile.prepare_jobs, 8);
        assert_eq!(batch.stats.profile.prepare_errors, 0);
        assert!(batch.stats.errors.is_empty());
        assert_eq!(
            batch
                .stats
                .items
                .iter()
                .map(|item| item.path.clone())
                .collect::<Vec<_>>(),
            expected_paths
        );
        drop(pool);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn query_mode_parses_aliases() {
        assert_eq!(QueryMode::parse("all").unwrap(), QueryMode::All);
        assert_eq!(QueryMode::parse("pdq").unwrap(), QueryMode::Near);
        assert_eq!(QueryMode::parse("crop").unwrap(), QueryMode::Patch);
        assert_eq!(QueryMode::parse("screen").unwrap(), QueryMode::Screenshot);
        assert_eq!(QueryMode::parse("strict").unwrap(), QueryMode::Strict);
        assert_eq!(QueryMode::parse("recall").unwrap(), QueryMode::Recall);
        assert!(QueryMode::parse("bad").is_err());
    }

    #[test]
    fn query_strategy_defaults_to_exact_and_parses_fast_aliases() {
        assert_eq!(QueryStrategy::default(), QueryStrategy::Exact);
        assert_eq!(QueryStrategy::parse("").unwrap(), QueryStrategy::Exact);
        assert_eq!(
            QueryStrategy::parse("full-scan").unwrap(),
            QueryStrategy::Exact
        );
        assert_eq!(QueryStrategy::parse("fast").unwrap(), QueryStrategy::Fast);
        assert_eq!(QueryStrategy::parse("bucket").unwrap(), QueryStrategy::Fast);
        assert!(QueryStrategy::parse("bad").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn wsl_mount_path_converts_windows_drive_paths() {
        assert_eq!(
            wsl_mount_path_from_windows_path(Path::new(r"\\?\D:\QQData\1234567890\Image\a.jpg"))
                .unwrap(),
            PathBuf::from("/mnt/d/QQData/1234567890/Image/a.jpg")
        );
        assert_eq!(
            wsl_mount_path_from_windows_path(Path::new(r"D:\QQData\a b\c.jpg")).unwrap(),
            PathBuf::from("/mnt/d/QQData/a b/c.jpg")
        );
        assert!(wsl_mount_path_from_windows_path(Path::new("/mnt/d/already/linux")).is_none());
    }

    #[test]
    fn build_stage_parses_aliases() {
        assert_eq!(BuildStage::default(), BuildStage::All);
        assert_eq!(BuildStage::parse("").unwrap(), BuildStage::All);
        assert_eq!(BuildStage::parse("manifest").unwrap(), BuildStage::Manifest);
        assert_eq!(BuildStage::parse("hashes").unwrap(), BuildStage::Manifest);
        assert_eq!(
            BuildStage::parse("embeddings").unwrap(),
            BuildStage::Embeddings
        );
        assert_eq!(
            BuildStage::parse("vectors").unwrap(),
            BuildStage::Embeddings
        );
        assert!(BuildStage::parse("bad").is_err());
    }

    #[test]
    fn manifest_mode_defaults_to_full_and_parses_fast_aliases() {
        assert_eq!(ManifestMode::default(), ManifestMode::Full);
        assert_eq!(ManifestMode::parse("").unwrap(), ManifestMode::Full);
        assert_eq!(ManifestMode::parse("quality").unwrap(), ManifestMode::Full);
        assert_eq!(ManifestMode::parse("fast").unwrap(), ManifestMode::Fast);
        assert_eq!(
            ManifestMode::parse("metadata-only").unwrap(),
            ManifestMode::Fast
        );
        assert!(ManifestMode::parse("bad").is_err());
    }

    #[test]
    fn model_key_uses_path_variant_only_for_default_model_name() {
        assert_eq!(
            model_key_with_path_variant("mobileclip2-s2", "mobileclip2-s2", Some("mobileclip2-s2")),
            "mobileclip2-s2"
        );
        assert_eq!(
            model_key_with_path_variant(
                "mobileclip2-s2",
                "mobileclip2-s2",
                Some("mobileclip2-s2-fp16-inputfp16")
            ),
            "mobileclip2-s2-fp16-inputfp16"
        );
        assert_eq!(
            model_key_with_path_variant(
                "mobileclip2-s2",
                "explicit-key",
                Some("mobileclip2-s2-fp16-inputfp16")
            ),
            "explicit-key"
        );
        assert_eq!(
            model_key_with_path_variant(
                "sscd_disc_mixup",
                "",
                Some("sscd_disc_mixup_fp16_inputfp16")
            ),
            "sscd_disc_mixup_fp16_inputfp16"
        );
        assert_eq!(
            model_key_with_path_variant("sscd_disc_mixup", "sscd_disc_mixup", Some("model")),
            "sscd_disc_mixup"
        );
    }

    #[cfg(feature = "image-index-sscd")]
    #[test]
    fn sscd_preprocess_pads_static_tensor_batch() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            12,
            image::Rgb([128, 64, 32]),
        ));
        let (input, profile) = sscd_preprocess_batch_with_workers(&[image], 1, 2).unwrap();
        assert_eq!(input.shape(), &[2, 3, 320, 320]);
        assert!(profile.resize_cpu_ms > 0.0);
        assert!(input
            .index_axis(ndarray::Axis(0), 0)
            .iter()
            .any(|value| *value != 0.0));
        assert!(input
            .index_axis(ndarray::Axis(0), 1)
            .iter()
            .all(|value| *value == 0.0));
    }

    #[cfg(feature = "image-index-sscd")]
    #[test]
    fn sscd_vectors_reject_non_finite_output() {
        let vectors = normalize_sscd_vectors(1, 2, &[3.0, 4.0]).unwrap();
        assert!((vectors[0][0] - 0.6).abs() < 1e-6);
        assert!((vectors[0][1] - 0.8).abs() < 1e-6);
        assert!(normalize_sscd_vectors(1, 2, &[f32::NAN, 1.0]).is_err());
        assert!(normalize_sscd_vectors(1, 2, &[0.0, 0.0]).is_err());
    }

    #[test]
    fn manifest_benchmark_mode_parses_aliases() {
        assert_eq!(
            ManifestBenchmarkMode::parse("enum").unwrap(),
            ManifestBenchmarkMode::EnumOnly
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("open0").unwrap(),
            ManifestBenchmarkMode::EnumPlusOpen0
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("fileinfo").unwrap(),
            ManifestBenchmarkMode::EnumPlusFileInfo
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("header").unwrap(),
            ManifestBenchmarkMode::EnumPlusHeader
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("decode").unwrap(),
            ManifestBenchmarkMode::EnumPlusDecode
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("no-sqlite").unwrap(),
            ManifestBenchmarkMode::FullManifestNoSqlite
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("fast-fingerprint-no-sqlite").unwrap(),
            ManifestBenchmarkMode::FingerprintNoSqlite
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("turbojpeg-fingerprint-no-sqlite").unwrap(),
            ManifestBenchmarkMode::TurboJpegFingerprintNoSqlite
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("sqlite").unwrap(),
            ManifestBenchmarkMode::SqliteBatch
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("decode-profile").unwrap(),
            ManifestBenchmarkMode::DecodeProfile
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("zune-jpeg-luma-decode-profile").unwrap(),
            ManifestBenchmarkMode::JpegLumaDecodeProfile
        );
        assert_eq!(
            ManifestBenchmarkMode::parse("libjpeg-turbo-decode-profile").unwrap(),
            ManifestBenchmarkMode::TurboJpegDecodeProfile
        );
        assert!(ManifestBenchmarkMode::parse("bad").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn turbojpeg_benchmark_modes_require_backend() {
        for mode in [
            ManifestBenchmarkMode::TurboJpegDecodeProfile,
            ManifestBenchmarkMode::TurboJpegFingerprintNoSqlite,
        ] {
            let err = benchmark_manifest(ManifestBenchmarkOptions {
                root: unique_temp_root("image-index-missing-turbojpeg"),
                account: "bench".to_string(),
                asset_roots: Vec::new(),
                max_files: 1,
                mode,
                header_bytes: 0,
                sqlite_path: None,
                batch_size: 0,
            })
            .unwrap_err();
            assert!(err.to_string().contains("TurboJPEG benchmark"));
        }
    }

    #[test]
    fn query_limit_is_bounded_before_top_k_expansion() {
        assert_eq!(normalize_query_limit(0), 1);
        assert_eq!(normalize_query_limit(25), 25);
        assert_eq!(
            normalize_query_limit(MAX_QUERY_RESULTS + 1),
            MAX_QUERY_RESULTS
        );
        assert_eq!(normalize_query_limit(usize::MAX), MAX_QUERY_RESULTS);
        assert_eq!(
            expanded_query_limit(usize::MAX),
            MAX_QUERY_RESULTS * QUERY_CANDIDATE_MULTIPLIER
        );
    }

    #[test]
    fn sscd_scores_at_or_above_98_percent_are_same_image() {
        assert_eq!(sscd_match_kind(0.979_999), "copy_sscd");
        assert_eq!(sscd_match_kind(0.98), "same_sscd");
        assert_eq!(sscd_match_kind(1.0), "same_sscd");
    }

    #[test]
    fn hex_bytes_is_lowercase() {
        assert_eq!(hex_bytes(&[0, 15, 16, 255]), "000f10ff");
    }

    #[test]
    fn fingerprint_image_for_index_hashes_and_decodes_from_same_bytes() {
        let root = unique_temp_root("image-index-fingerprint-bytes");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.png");
        write_test_png(&path, [17, 34, 51]);

        let (fingerprint, image) = fingerprint_image_for_index(&path, true).unwrap();

        assert_eq!(fingerprint.sha256, sha256_file(&path).unwrap());
        assert_eq!(fingerprint.width, 8);
        assert_eq!(fingerprint.height, 8);
        assert_eq!(fingerprint.source_class, "image");
        assert_eq!(fingerprint.phash_algo, PHASH_ALGO_V2);
        assert_eq!(fingerprint.blur_algo, BLUR_ALGO_V2);
        assert_eq!(fingerprint.fingerprint_version, FINGERPRINT_VERSION_V2);
        assert_eq!(fingerprint.detected_format, "PNG");
        assert!(!fingerprint.has_alpha);
        assert!(!fingerprint.orientation_applied);
        assert!(fingerprint.quality_flags.contains("small"));
        assert!(fingerprint.quality_flags.contains("tiny"));
        assert!(!fingerprint.quality_flags.contains("blurry"));
        assert_eq!(fingerprint.tile_hashes.len(), tile_regions().len());
        assert!(image.is_some());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn jpeg_fast_fingerprint_matches_keep_image_path() {
        let root = unique_temp_root("image-index-jpeg-fast-fingerprint");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.jpg");
        write_test_jpeg(&path);

        let (fast, no_image) = fingerprint_image_for_index(&path, false).unwrap();
        let (with_image, image) = fingerprint_image_for_index(&path, true).unwrap();

        assert!(no_image.is_none());
        assert!(image.is_some());
        assert_eq!(fast.sha256, sha256_file(&path).unwrap());
        assert_eq!(fast.sha256, with_image.sha256);
        assert_eq!(fast.phash, with_image.phash);
        assert_eq!(fast.width, 16);
        assert_eq!(fast.height, 16);
        assert_eq!(fast.width, with_image.width);
        assert_eq!(fast.height, with_image.height);
        assert_eq!(fast.quality_flags, with_image.quality_flags);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn phash64_hex_is_stable_for_test_image() {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(16, 16, |x, y| {
            image::Rgb([
                ((x * 17 + y * 3) % 256) as u8,
                ((x * 5 + y * 11) % 256) as u8,
                ((x * 13 + y * 7) % 256) as u8,
            ])
        }));

        assert_eq!(phash64_hex(&image), "25da25fb14fd1a20");
    }

    #[test]
    fn dot_f32_blob_rejects_wrong_dimensions() {
        let blob = encode_f32_vec(&[0.25, 0.75]);
        assert_eq!(dot_f32_blob(&[2.0, 4.0], &blob), Some(3.5));
        assert_eq!(dot_f32_blob(&[2.0], &blob), None);
    }

    #[test]
    fn vector_sketch64_matches_uncached_projection() {
        let vector = [0.25, -0.5, 0.75, 1.25, -1.5, 0.0, 0.125];
        let expected = vector_sketch64_uncached_for_test(&vector);

        assert_eq!(vector_sketch64(&vector), expected);
        assert_eq!(vector_sketch64(&vector), expected);
        assert_eq!(
            vector_sketch64_blob(&encode_f32_vec(&vector)),
            Some(expected)
        );
    }

    #[test]
    fn parallel_embedding_value_prepare_matches_serial_order() {
        let vectors = (0..97)
            .map(|row| {
                (0..(17 + row % 11))
                    .map(|column| ((row * 31 + column * 17) as f32 - 700.0) / 113.0)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let parallel = prepare_embedding_values(&vectors).unwrap();

        assert_eq!(parallel.len(), vectors.len());
        for (vector, actual) in vectors.iter().zip(&parallel) {
            let expected = prepare_embedding_value(vector);
            assert_eq!(actual.dim, expected.dim);
            assert_eq!(actual.blob, expected.blob);
            assert_eq!(actual.sketch_hex, expected.sketch_hex);
            assert_eq!(actual.bucket, expected.bucket);
        }
    }

    #[test]
    fn image_file_walker_finds_nested_supported_images() {
        let root = unique_temp_root("image-file-walker");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        write_test_png(&root.join("a.png"), [255, 0, 0]);
        std::fs::write(nested.join("b.webp"), b"supported extension").unwrap();
        std::fs::write(root.join("notes.txt"), b"not an image").unwrap();

        let mut names = ImageFileWalker::new(&root)
            .map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["a.png".to_string(), "b.webp".to_string()]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scan_roots_uses_explicit_asset_roots_without_account_root() {
        let root = unique_temp_root("image-index-explicit-roots");
        let asset_root = root.join("assets");
        let account_root = root.join("bench");
        std::fs::create_dir_all(&asset_root).unwrap();
        std::fs::create_dir_all(&account_root).unwrap();

        let roots = scan_roots(&BuildOptions {
            root: root.clone(),
            account: "bench".to_string(),
            asset_roots: vec![asset_root.clone()],
            manifest_path: None,
            max_files: 10,
            force: false,
            pipeline: "full".to_string(),
            stage: BuildStage::Manifest,
            model_dir: None,
            sscd_model_dir: None,
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "cpu".to_string(),
            backend: "manifest-sqlite".to_string(),
            clip_batch_size: 1,
            manifest_workers: 1,
            manifest_mode: ManifestMode::Full,
        });

        assert_eq!(roots, vec![asset_root.canonicalize().unwrap()]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_index_can_use_explicit_manifest_path() {
        let root = unique_temp_root("image-index-build-explicit-manifest");
        let asset_root = root.join("assets");
        let manifest = root.join("fast-storage").join("manifest.sqlite");
        std::fs::create_dir_all(&asset_root).unwrap();
        write_test_png(&asset_root.join("valid.png"), [0, 128, 255]);

        let mut options = test_build_options(&root, "bench", &asset_root, 10);
        options.stage = BuildStage::Manifest;
        options.manifest_path = Some(manifest.clone());
        let report = build_index(options).unwrap();

        assert_eq!(report.manifest, manifest.display().to_string());
        assert_eq!(
            report.index_root,
            manifest.parent().unwrap().display().to_string()
        );
        assert!(manifest.is_file());
        assert!(!default_manifest_path(&root, "bench").exists());
        let status = status_with_manifest(&root, "bench", Some(&manifest)).unwrap();
        assert_eq!(status.images, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_benchmark_splits_enum_decode_and_sqlite_costs() {
        let root = unique_temp_root("image-index-manifest-bench");
        let asset_root = root.join("assets");
        std::fs::create_dir_all(&asset_root).unwrap();
        write_test_png(&asset_root.join("valid.png"), [255, 0, 0]);
        std::fs::write(asset_root.join("broken.jpg"), b"not a jpeg").unwrap();
        std::fs::write(asset_root.join("notes.txt"), b"not an image").unwrap();

        let enum_report = benchmark_manifest(ManifestBenchmarkOptions {
            root: root.clone(),
            account: "bench".to_string(),
            asset_roots: vec![asset_root.clone()],
            max_files: 10,
            mode: ManifestBenchmarkMode::EnumOnly,
            header_bytes: 0,
            sqlite_path: None,
            batch_size: 0,
        })
        .unwrap();
        assert_eq!(enum_report.supported_files, 2);
        assert_eq!(enum_report.processed_files, 2);
        assert_eq!(enum_report.decode_count, 0);
        assert_eq!(enum_report.sqlite_insert_count, 0);

        let decode_report = benchmark_manifest(ManifestBenchmarkOptions {
            root: root.clone(),
            account: "bench".to_string(),
            asset_roots: vec![asset_root.clone()],
            max_files: 10,
            mode: ManifestBenchmarkMode::EnumPlusDecode,
            header_bytes: 0,
            sqlite_path: None,
            batch_size: 0,
        })
        .unwrap();
        assert_eq!(decode_report.supported_files, 2);
        assert_eq!(decode_report.decode_count, 2);
        assert_eq!(decode_report.decode_errors, 1);
        assert!(decode_report.bytes_read > 0);

        let sqlite_path = root.join("bench.sqlite");
        let sqlite_report = benchmark_manifest(ManifestBenchmarkOptions {
            root: root.clone(),
            account: "bench".to_string(),
            asset_roots: vec![asset_root.clone()],
            max_files: 10,
            mode: ManifestBenchmarkMode::SqliteBatch,
            header_bytes: 0,
            sqlite_path: Some(sqlite_path.clone()),
            batch_size: 1,
        })
        .unwrap();
        assert_eq!(sqlite_report.supported_files, 2);
        assert_eq!(sqlite_report.sqlite_insert_count, 2);
        assert!(sqlite_path.is_file());

        let profile_report = benchmark_manifest(ManifestBenchmarkOptions {
            root: root.clone(),
            account: "bench".to_string(),
            asset_roots: vec![asset_root],
            max_files: 10,
            mode: ManifestBenchmarkMode::DecodeProfile,
            header_bytes: 0,
            sqlite_path: None,
            batch_size: 0,
        })
        .unwrap();
        let profile = profile_report.decode_profile.unwrap();
        let png = profile
            .by_format
            .iter()
            .find(|row| row.format == "PNG")
            .unwrap();
        assert_eq!(png.count, 1);
        assert_eq!(png.error_count, 0);
        assert!(png.total_pixels > 0);
        let jpeg = profile
            .by_format
            .iter()
            .find(|row| row.format == "JPEG")
            .unwrap();
        assert_eq!(jpeg.count, 1);
        assert_eq!(jpeg.error_count, 1);
        assert!(profile
            .by_format_file_size
            .iter()
            .any(|row| row.format == "PNG" && row.file_size_bucket.is_some()));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn init_schema_migrates_old_embedding_table_for_vector_buckets() {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch(
            r#"
            create table image_embeddings (
                path text not null,
                kind text not null,
                model text not null,
                dim integer not null,
                vec blob not null,
                updated_at text not null default '',
                primary key(path, kind, model)
            );
            "#,
        )
        .unwrap();
        init_schema(&con).unwrap();
        let columns = table_columns(&con, "image_embeddings");
        assert!(columns.iter().any(|column| column == "sketch64_hex"));
        assert!(columns.iter().any(|column| column == "bucket12"));
    }

    #[test]
    fn init_schema_migrates_old_asset_table_for_quality_metadata() {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch(
            r#"
            create table image_assets (
                id integer primary key autoincrement,
                path text not null unique,
                source_root text not null default '',
                file_size integer not null default 0,
                mtime_unix integer not null default 0,
                sha256_hex text not null default '',
                phash_hex text,
                width integer,
                height integer,
                indexed_at text not null default '',
                stale integer not null default 0,
                error text
            );
            "#,
        )
        .unwrap();
        init_schema(&con).unwrap();
        let columns = table_columns(&con, "image_assets");
        assert!(columns.iter().any(|column| column == "blur_score"));
        assert!(columns.iter().any(|column| column == "quality_flags"));
        assert!(columns.iter().any(|column| column == "source_class"));
    }

    #[test]
    fn init_schema_configures_manifest_connection_pragmas() {
        let con = test_manifest().unwrap();
        let busy_timeout: i64 = con
            .query_row("pragma busy_timeout", [], |row| row.get(0))
            .unwrap();
        let synchronous: i64 = con
            .query_row("pragma synchronous", [], |row| row.get(0))
            .unwrap();
        let temp_store: i64 = con
            .query_row("pragma temp_store", [], |row| row.get(0))
            .unwrap();

        assert_eq!(busy_timeout, 5000);
        assert_eq!(synchronous, 1);
        assert_eq!(temp_store, 2);
    }

    #[test]
    fn upsert_embedding_writes_vector_bucket_metadata() {
        let con = test_manifest().unwrap();
        insert_test_asset(&con, "a.png", "0000000000000000").unwrap();
        upsert_embedding(&con, "a.png", "clip", "m", &[1.0, 0.0]).unwrap();
        let (sketch, bucket): (String, i64) = con
            .query_row(
                "select sketch64_hex, bucket12 from image_embeddings where path='a.png'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(sketch, format!("{:016x}", vector_sketch64(&[1.0, 0.0])));
        assert_eq!(bucket, vector_bucket12(vector_sketch64(&[1.0, 0.0])) as i64);
    }

    #[test]
    fn reuse_duplicate_embeddings_copies_exact_duplicate_vectors() {
        let con = test_manifest().unwrap();
        con.execute(
            "insert into image_assets (path, sha256_hex, phash_hex, width, height, source_class, stale) values ('a.png', 'sha-same', '0000000000000000', 8, 8, 'image', 0)",
            [],
        )
        .unwrap();
        con.execute(
            "insert into image_assets (path, sha256_hex, phash_hex, width, height, source_class, stale) values ('b.png', 'sha-same', '0000000000000000', 8, 8, 'image', 0)",
            [],
        )
        .unwrap();
        upsert_embedding(&con, "a.png", "clip", "m", &[1.0, 0.0]).unwrap();

        assert_eq!(
            reuse_duplicate_embeddings(&con, &[("clip".to_string(), "m".to_string())]).unwrap(),
            1
        );
        assert!(embedding_exists(&con, "b.png", "clip", "m").unwrap());
    }

    #[test]
    fn backfill_embedding_metadata_uses_existing_vectors_without_reembedding() {
        let mut con = test_manifest().unwrap();
        insert_test_asset(&con, "a.png", "0000000000000000").unwrap();
        con.execute(
            "insert into image_embeddings (path, kind, model, dim, vec) values (?1, ?2, ?3, ?4, ?5)",
            params!["a.png", "clip", "m", 2_i64, encode_f32_vec(&[1.0, 0.0])],
        )
        .unwrap();
        assert!(!embedding_exists(&con, "a.png", "clip", "m").unwrap());

        assert_eq!(backfill_embedding_metadata(&mut con).unwrap(), 1);
        assert!(embedding_exists(&con, "a.png", "clip", "m").unwrap());
        assert_eq!(backfill_embedding_metadata(&mut con).unwrap(), 0);
    }

    #[test]
    fn backfill_embedding_metadata_marks_invalid_blobs_without_looping() {
        let mut con = test_manifest().unwrap();
        insert_test_asset(&con, "bad.png", "0000000000000000").unwrap();
        con.execute(
            "insert into image_embeddings (path, kind, model, dim, vec) values (?1, ?2, ?3, ?4, ?5)",
            params!["bad.png", "clip", "m", 2_i64, vec![1_u8, 2, 3]],
        )
        .unwrap();

        assert_eq!(backfill_embedding_metadata(&mut con).unwrap(), 0);
        let (sketch, bucket): (String, i64) = con
            .query_row(
                "select sketch64_hex, bucket12 from image_embeddings where path='bad.png'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(sketch, "invalid");
        assert_eq!(bucket, -1);
        assert!(!embedding_exists(&con, "bad.png", "clip", "m").unwrap());
    }

    #[test]
    fn embedding_exists_accepts_legacy_path_model_key() {
        let con = test_manifest().unwrap();
        insert_test_embedding(
            &con,
            "legacy.png",
            "clip",
            "m:output/_deps/models/m",
            &[1.0, 0.0],
        )
        .unwrap();

        assert!(embedding_exists(&con, "legacy.png", "clip", "m").unwrap());
        assert!(!embedding_exists(&con, "legacy.png", "clip", "other").unwrap());
    }

    #[test]
    fn upsert_embedding_removes_same_path_legacy_model_key() {
        let con = test_manifest().unwrap();
        insert_test_embedding(&con, "same.png", "clip", "m:old/model/path", &[0.8, 0.0]).unwrap();
        upsert_embedding(&con, "same.png", "clip", "m", &[0.9, 0.0]).unwrap();

        let models = con
            .prepare("select model from image_embeddings where path='same.png' order by model")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(models, vec!["m".to_string()]);
    }

    #[test]
    fn canonicalize_legacy_model_keys_moves_legacy_rows_to_current_model() {
        let mut con = test_manifest().unwrap();
        insert_test_asset(&con, "legacy.png", "0000000000000000").unwrap();
        insert_raw_embedding(
            &con,
            "legacy.png",
            "clip",
            "m:output/_deps/models/m",
            &[0.95, 0.0],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        assert_eq!(
            canonicalize_legacy_model_keys(&mut con, &[("clip".to_string(), "m".to_string())])
                .unwrap(),
            1
        );
        assert!(embedding_exists(&con, "legacy.png", "clip", "m").unwrap());
        let models = models_for_path(&con, "legacy.png");
        assert_eq!(models, vec!["m".to_string()]);

        let results =
            semantic_matches(&con, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 1).unwrap();
        assert_eq!(results[0].0.path, PathBuf::from("legacy.png"));
        assert_eq!((results[0].1 * 1000.0).round() as i64, 950);
    }

    #[test]
    fn canonicalize_legacy_model_keys_keeps_newest_conflicting_embedding() {
        let mut con = test_manifest().unwrap();
        insert_test_asset(&con, "same.png", "0000000000000000").unwrap();
        insert_raw_embedding(
            &con,
            "same.png",
            "clip",
            "m",
            &[0.8, 0.0],
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        insert_raw_embedding(
            &con,
            "same.png",
            "clip",
            "m:old/model/path",
            &[0.95, 0.0],
            "2026-01-02T00:00:00Z",
        )
        .unwrap();

        assert_eq!(
            canonicalize_legacy_model_keys(&mut con, &[("clip".to_string(), "m".to_string())])
                .unwrap(),
            1
        );
        assert_eq!(models_for_path(&con, "same.png"), vec!["m".to_string()]);

        let results =
            semantic_matches(&con, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 1).unwrap();
        assert_eq!((results[0].1 * 1000.0).round() as i64, 950);
    }

    #[test]
    fn import_embeddings_copies_matching_rows_without_dead_schema() {
        let root = unique_temp_root("image-index-import-embeddings");
        std::fs::create_dir_all(&root).unwrap();
        let source_path = root.join("source.sqlite");
        let target_path = root.join("target.sqlite");

        {
            let source = Connection::open(&source_path).unwrap();
            init_schema(&source).unwrap();
            insert_test_asset(&source, "same.png", "0000000000000000").unwrap();
            insert_raw_embedding(
                &source,
                "same.png",
                "clip",
                "m",
                &[0.9, 0.0],
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
            insert_test_asset(&source, "existing.png", "0000000000000000").unwrap();
            insert_raw_embedding(
                &source,
                "existing.png",
                "clip",
                "m",
                &[0.8, 0.0],
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
            insert_test_asset(&source, "mismatch.png", "0000000000000000").unwrap();
            insert_raw_embedding(
                &source,
                "mismatch.png",
                "clip",
                "m",
                &[0.7, 0.0],
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        }
        {
            let target = Connection::open(&target_path).unwrap();
            init_schema(&target).unwrap();
            assert!(!attached_table_exists(&target, "main", "image_asset_embeddings").unwrap());
            insert_test_asset(&target, "same.png", "0000000000000000").unwrap();
            insert_test_embedding(&target, "existing.png", "clip", "m", &[0.2, 0.0]).unwrap();
            insert_test_asset(&target, "mismatch.png", "0000000000000000").unwrap();
            target
                .execute(
                    "update image_assets set sha256_hex='different-sha' where path='mismatch.png'",
                    [],
                )
                .unwrap();
        }

        let report = import_embeddings(ImportEmbeddingsOptions {
            root: root.clone(),
            account: "import".to_string(),
            source_manifest: source_path.clone(),
            target_manifest: Some(target_path.clone()),
            force: false,
        })
        .unwrap();
        assert_eq!(report.source_rows, 3);
        assert_eq!(report.matching_rows, 2);
        assert_eq!(report.imported_rows, 1);
        assert_eq!(report.skipped_existing_rows, 1);

        let target = Connection::open(&target_path).unwrap();
        init_schema(&target).unwrap();
        let results =
            semantic_matches(&target, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 10).unwrap();
        let scores = results
            .iter()
            .map(|(item, score)| {
                (
                    item.path.to_string_lossy().to_string(),
                    (*score * 1000.0).round() as i64,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            scores,
            vec![
                ("same.png".to_string(), 900),
                ("existing.png".to_string(), 200)
            ]
        );

        let force_report = import_embeddings(ImportEmbeddingsOptions {
            root: root.clone(),
            account: "import".to_string(),
            source_manifest: source_path,
            target_manifest: Some(target_path.clone()),
            force: true,
        })
        .unwrap();
        assert_eq!(force_report.matching_rows, 2);
        assert_eq!(force_report.imported_rows, 2);

        let target = Connection::open(&target_path).unwrap();
        init_schema(&target).unwrap();
        let results =
            semantic_matches(&target, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 10).unwrap();
        let scores = results
            .iter()
            .map(|(item, score)| {
                (
                    item.path.to_string_lossy().to_string(),
                    (*score * 1000.0).round() as i64,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            scores,
            vec![
                ("same.png".to_string(), 900),
                ("existing.png".to_string(), 800)
            ]
        );
        assert_eq!(
            count_i64(
                &target,
                "select count(*) from image_embeddings where path='mismatch.png'"
            )
            .unwrap(),
            0
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mark_missing_assets_stale_marks_only_unseen_active_assets() {
        let mut con = test_manifest().unwrap();
        insert_test_asset(&con, "seen.png", "0000000000000000").unwrap();
        insert_test_asset(&con, "missing.png", "0000000000000000").unwrap();
        con.execute(
            "insert into image_assets (path, sha256_hex, stale) values ('old-stale.png', 'sha-old', 1)",
            [],
        )
        .unwrap();

        reset_seen_paths(&con).unwrap();
        assert!(remember_seen_path(&con, "seen.png").unwrap());
        assert!(!remember_seen_path(&con, "seen.png").unwrap());
        assert_eq!(mark_missing_assets_stale(&mut con).unwrap(), 1);

        let active = active_asset_paths(&con);
        assert_eq!(active, vec!["seen.png".to_string()]);
        assert_eq!(
            count_i64(&con, "select count(*) from image_assets where stale!=0").unwrap(),
            2
        );
    }

    #[test]
    fn build_index_marks_deleted_files_stale_after_complete_scan() {
        let root = unique_temp_root("image-index-stale-build");
        let account = "stale_account";
        let asset_root = root.join("fixtures");
        std::fs::create_dir_all(&asset_root).unwrap();
        write_test_png(&asset_root.join("a.png"), [255, 0, 0]);
        write_test_png(&asset_root.join("b.png"), [0, 255, 0]);

        let first = build_index(test_build_options(&root, account, &asset_root, 100)).unwrap();
        assert_eq!(first.indexed_files, 2);
        assert_eq!(first.stale_files, 0);

        std::fs::remove_file(asset_root.join("b.png")).unwrap();
        let second = build_index(test_build_options(&root, account, &asset_root, 100)).unwrap();
        assert_eq!(second.scanned_files, 1);
        assert_eq!(second.unchanged_files, 1);
        assert_eq!(second.stale_files, 1);

        let report = status(&root, account).unwrap();
        assert_eq!(report.images, 1);
        assert_eq!(report.stale_images, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_stage_records_quality_without_embeddings() {
        let root = unique_temp_root("image-index-manifest-stage");
        let account = "manifest_stage_account";
        let asset_root = root.join("fixtures");
        std::fs::create_dir_all(&asset_root).unwrap();
        write_test_png(&asset_root.join("tiny.png"), [12, 12, 12]);

        let mut options = test_build_options(&root, account, &asset_root, 100);
        options.stage = BuildStage::Manifest;
        let report = build_index(options).unwrap();
        assert_eq!(report.stage, "manifest");
        assert_eq!(report.indexed_files, 1);
        assert_eq!(report.embedded_files, 0);
        assert_eq!(report.quality.active_images, 1);
        assert_eq!(report.quality.small_files, 1);
        assert_eq!(report.quality.tiny_files, 1);
        assert_eq!(report.quality.blurry_files, 0);
        assert_eq!(report.root_stats.len(), 1);
        assert!(report.root_stats[0].directories_seen >= 1);

        let status = status(&root, account).unwrap();
        assert_eq!(status.quality.active_images, 1);
        assert_eq!(status.quality.small_files, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parallel_manifest_stage_indexes_successes_and_errors() {
        let root = unique_temp_root("image-index-parallel-manifest");
        let account = "parallel_manifest_account";
        let asset_root = root.join("fixtures");
        std::fs::create_dir_all(&asset_root).unwrap();
        write_test_png(&asset_root.join("a.png"), [255, 0, 0]);
        write_test_png(&asset_root.join("b.png"), [0, 255, 0]);
        std::fs::write(asset_root.join("broken.jpg"), b"not a jpeg").unwrap();

        let mut options = test_build_options(&root, account, &asset_root, 100);
        options.stage = BuildStage::Manifest;
        options.manifest_workers = 2;
        let report = build_index(options).unwrap();

        assert_eq!(report.manifest_workers, 2);
        assert_eq!(report.scanned_files, 3);
        assert_eq!(report.indexed_files, 2);
        assert_eq!(report.error_files, 1);
        assert_eq!(report.root_stats.len(), 1);
        assert_eq!(report.root_stats[0].indexed_files, 2);
        assert_eq!(report.root_stats[0].error_files, 1);

        let status = status(&root, account).unwrap();
        assert_eq!(status.images, 3);
        assert_eq!(status.errors, 1);
        assert_eq!(status.perceptual_hashes, 2);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fast_manifest_stage_defers_quality_until_full_manifest() {
        let root = unique_temp_root("image-index-fast-manifest");
        let account = "fast_manifest_account";
        let asset_root = root.join("fixtures");
        std::fs::create_dir_all(&asset_root).unwrap();
        write_test_png(&asset_root.join("a.png"), [255, 0, 0]);
        write_test_png(&asset_root.join("b.png"), [0, 255, 0]);
        std::fs::write(asset_root.join("broken.jpg"), b"not a jpeg").unwrap();

        let mut fast_options = test_build_options(&root, account, &asset_root, 100);
        fast_options.stage = BuildStage::Manifest;
        fast_options.manifest_mode = ManifestMode::Fast;
        let fast = build_index(fast_options).unwrap();

        assert_eq!(fast.manifest_mode, "fast");
        assert_eq!(fast.scanned_files, 3);
        assert_eq!(fast.indexed_files, 3);
        assert_eq!(fast.error_files, 0);
        assert_eq!(fast.quality.active_images, 3);
        assert_eq!(fast.quality.manifest_pending_files, 3);
        assert_eq!(fast.quality.exact_phash_files, 0);

        let fast_status = status(&root, account).unwrap();
        assert_eq!(fast_status.errors, 0);
        assert_eq!(fast_status.exact_hashes, 0);
        assert_eq!(fast_status.perceptual_hashes, 0);
        assert_eq!(fast_status.quality.manifest_pending_files, 3);

        let mut full_options = test_build_options(&root, account, &asset_root, 100);
        full_options.stage = BuildStage::Manifest;
        full_options.force = true;
        let full = build_index(full_options).unwrap();

        assert_eq!(full.manifest_mode, "full");
        assert_eq!(full.scanned_files, 3);
        assert_eq!(full.indexed_files, 2);
        assert_eq!(full.error_files, 1);
        assert_eq!(full.quality.manifest_pending_files, 0);

        let full_status = status(&root, account).unwrap();
        assert_eq!(full_status.errors, 1);
        assert_eq!(full_status.perceptual_hashes, 2);
        assert_eq!(full_status.quality.manifest_pending_files, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn query_patch_mode_uses_tile_hash_for_crop() {
        let root = unique_temp_root("image-index-patch-tile-query");
        let account = "patch_tile_account";
        let asset_root = root.join("fixtures");
        std::fs::create_dir_all(&asset_root).unwrap();
        let base_path = asset_root.join("base.png");
        let crop_path = root.join("query-crop.png");
        write_patch_fixture(&base_path, &crop_path);

        let mut options = test_build_options(&root, account, &asset_root, 100);
        options.stage = BuildStage::Manifest;
        let build = build_index(options).unwrap();
        assert_eq!(build.indexed_files, 1);

        let manifest = default_manifest_path(&root, account);
        let con = Connection::open(&manifest).unwrap();
        init_schema(&con).unwrap();
        assert_eq!(
            count_i64(&con, "select count(*) from image_asset_tile_hashes").unwrap(),
            tile_regions().len() as i64
        );
        drop(con);

        let report = query_image(QueryImageOptions {
            root: root.clone(),
            account: account.to_string(),
            manifest_path: None,
            image: crop_path,
            mode: QueryMode::Patch,
            query_strategy: QueryStrategy::Exact,
            limit: 5,
            hamming_threshold: DEFAULT_HAMMING_THRESHOLD,
            model_dir: None,
            sscd_model_dir: None,
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "cpu".to_string(),
        })
        .unwrap();

        assert!(report.unavailable.is_empty());
        assert!(!report.results.is_empty());
        let best = &report.results[0];
        assert_eq!(best.match_kind, "tile_hash");
        assert_eq!(best.match_source, "tile_hash");
        assert_eq!(best.path, base_path.to_string_lossy());
        assert_eq!(best.tile_algo.as_deref(), Some(TILE_ALGO_V1));
        assert!(best.distance.unwrap_or(u32::MAX) <= TILE_DEFAULT_HAMMING_THRESHOLD);
        assert!(best.matched_tile_count.unwrap_or(0) > 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_index_does_not_mark_stale_when_file_limit_is_hit() {
        let root = unique_temp_root("image-index-stale-limit");
        let account = "stale_limit_account";
        let asset_root = root.join("fixtures");
        std::fs::create_dir_all(&asset_root).unwrap();
        write_test_png(&asset_root.join("a.png"), [255, 0, 0]);
        write_test_png(&asset_root.join("b.png"), [0, 255, 0]);
        write_test_png(&asset_root.join("c.png"), [0, 0, 255]);

        build_index(test_build_options(&root, account, &asset_root, 100)).unwrap();
        std::fs::remove_file(asset_root.join("c.png")).unwrap();

        let second = build_index(test_build_options(&root, account, &asset_root, 1)).unwrap();
        assert_eq!(second.scanned_files, 1);
        assert_eq!(second.stale_files, 0);

        let report = status(&root, account).unwrap();
        assert_eq!(report.images, 3);
        assert_eq!(report.stale_images, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_index_marks_stale_when_scan_count_equals_limit_but_no_files_remain() {
        let root = unique_temp_root("image-index-stale-exact-limit");
        let account = "stale_exact_limit_account";
        let asset_root = root.join("fixtures");
        std::fs::create_dir_all(&asset_root).unwrap();
        write_test_png(&asset_root.join("a.png"), [255, 0, 0]);
        write_test_png(&asset_root.join("b.png"), [0, 255, 0]);

        build_index(test_build_options(&root, account, &asset_root, 100)).unwrap();
        std::fs::remove_file(asset_root.join("b.png")).unwrap();

        let second = build_index(test_build_options(&root, account, &asset_root, 1)).unwrap();
        assert_eq!(second.scanned_files, 1);
        assert_eq!(second.unchanged_files, 1);
        assert_eq!(second.stale_files, 1);

        let report = status(&root, account).unwrap();
        assert_eq!(report.images, 1);
        assert_eq!(report.stale_images, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn query_exact_uses_sha256_without_decoding_query_image() {
        let root = unique_temp_root("image-index-exact-no-decode");
        let account = "exact_no_decode";
        std::fs::create_dir_all(&root).unwrap();
        let query_path = root.join("not-an-image.jpg");
        std::fs::write(&query_path, b"this is not decodable image data").unwrap();

        let manifest = default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let con = Connection::open(&manifest).unwrap();
        init_schema(&con).unwrap();
        let sha256 = sha256_file(&query_path).unwrap();
        con.execute(
            "insert into image_assets (path, sha256_hex, stale) values (?1, ?2, 0)",
            params![query_path.to_string_lossy().to_string(), sha256],
        )
        .unwrap();
        drop(con);

        let report = query_image(QueryImageOptions {
            root: root.clone(),
            account: account.to_string(),
            manifest_path: None,
            image: query_path.clone(),
            mode: QueryMode::Exact,
            query_strategy: QueryStrategy::Exact,
            limit: 5,
            hamming_threshold: DEFAULT_HAMMING_THRESHOLD,
            model_dir: None,
            sscd_model_dir: None,
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "cpu".to_string(),
        })
        .unwrap();

        assert_eq!(report.query.sha256, sha256_file(&query_path).unwrap());
        assert_eq!(report.query.phash, "");
        assert_eq!(report.query.width, 0);
        assert_eq!(report.query.height, 0);
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].match_kind, "exact");
        assert_eq!(report.results[0].path, query_path.to_string_lossy());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn query_and_status_can_use_explicit_manifest_path() {
        let root = unique_temp_root("image-index-explicit-manifest");
        std::fs::create_dir_all(&root).unwrap();
        let query_path = root.join("not-an-image.jpg");
        std::fs::write(&query_path, b"explicit manifest lookup").unwrap();
        let manifest = root.join("custom-manifest.sqlite");
        let con = Connection::open(&manifest).unwrap();
        init_schema(&con).unwrap();
        let sha256 = sha256_file(&query_path).unwrap();
        con.execute(
            "insert into image_assets (path, sha256_hex, stale) values (?1, ?2, 0)",
            params![query_path.to_string_lossy().to_string(), sha256],
        )
        .unwrap();
        drop(con);

        let status = status_with_manifest(&root, "real_account_label", Some(&manifest)).unwrap();
        assert_eq!(status.account, "real_account_label");
        assert_eq!(status.images, 1);

        let report = query_image(QueryImageOptions {
            root: root.clone(),
            account: "real_account_label".to_string(),
            manifest_path: Some(manifest),
            image: query_path.clone(),
            mode: QueryMode::Exact,
            query_strategy: QueryStrategy::Exact,
            limit: 5,
            hamming_threshold: DEFAULT_HAMMING_THRESHOLD,
            model_dir: None,
            sscd_model_dir: None,
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "cpu".to_string(),
        })
        .unwrap();
        assert_eq!(report.account, "real_account_label");
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].path, query_path.to_string_lossy());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn indexed_asset_similarity_reuses_manifest_vectors_without_source_image() {
        let root = unique_temp_root("image-index-stored-query-vectors");
        let account = "stored_query_vectors";
        let manifest = default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let con = Connection::open(&manifest).unwrap();
        init_schema(&con).unwrap();
        let query_path = root.join("source-file-does-not-exist.png");
        let candidate_path = root.join("candidate-also-does-not-exist.png");
        let query_text = query_path.to_string_lossy().to_string();
        let candidate_text = candidate_path.to_string_lossy().to_string();
        insert_test_asset(&con, &query_text, "0123456789abcdef").unwrap();
        insert_test_asset(&con, &candidate_text, "0123456789abcdef").unwrap();
        upsert_embedding(&con, &query_text, "clip", "test-clip", &[1.0, 0.0]).unwrap();
        upsert_embedding(&con, &candidate_text, "clip", "test-clip", &[0.9, 0.1]).unwrap();
        upsert_embedding(&con, &query_text, "sscd", "test-sscd", &[1.0, 0.0]).unwrap();
        upsert_embedding(&con, &candidate_text, "sscd", "test-sscd", &[0.8, 0.2]).unwrap();
        let asset_id = con
            .query_row(
                "select id from image_assets where path=?1",
                [&query_text],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        drop(con);

        assert!(!query_path.exists());
        let semantic = query_indexed_asset(
            &root,
            account,
            asset_id,
            QueryMode::Semantic,
            QueryStrategy::Exact,
            10,
        )
        .unwrap();
        assert!(semantic.unavailable.is_empty());
        assert!(semantic.results.iter().any(|result| {
            result.path == candidate_text && result.match_kind == "semantic_clip"
        }));

        let copy = query_indexed_asset(
            &root,
            account,
            asset_id,
            QueryMode::Copy,
            QueryStrategy::Exact,
            10,
        )
        .unwrap();
        assert!(copy.unavailable.is_empty());
        assert!(copy
            .results
            .iter()
            .any(|result| { result.path == candidate_text && result.match_kind == "copy_sscd" }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn query_all_skips_lower_priority_layers_when_exact_results_fill_limit() {
        let root = unique_temp_root("image-index-all-exact-short-circuit");
        let account = "all_exact_short_circuit";
        std::fs::create_dir_all(&root).unwrap();
        let query_path = root.join("query.png");
        let duplicate_path = root.join("duplicate.png");
        write_test_png(&query_path, [42, 84, 126]);
        std::fs::copy(&query_path, &duplicate_path).unwrap();

        let fingerprint = fingerprint_image_for_query(&query_path, &QueryMode::All)
            .unwrap()
            .0;
        let manifest = default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let con = Connection::open(&manifest).unwrap();
        init_schema(&con).unwrap();
        for path in [&query_path, &duplicate_path] {
            con.execute(
                "insert into image_assets (path, sha256_hex, phash_hex, width, height, stale) values (?1, ?2, ?3, ?4, ?5, 0)",
                params![
                    path.to_string_lossy().to_string(),
                    fingerprint.sha256.clone(),
                    fingerprint.phash.clone(),
                    fingerprint.width as i64,
                    fingerprint.height as i64,
                ],
            )
            .unwrap();
        }
        drop(con);

        let report = query_image(QueryImageOptions {
            root: root.clone(),
            account: account.to_string(),
            manifest_path: None,
            image: query_path.clone(),
            mode: QueryMode::All,
            query_strategy: QueryStrategy::Exact,
            limit: 2,
            hamming_threshold: DEFAULT_HAMMING_THRESHOLD,
            model_dir: None,
            sscd_model_dir: None,
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "cpu".to_string(),
        })
        .unwrap();

        assert_eq!(report.results.len(), 2);
        assert!(report
            .results
            .iter()
            .all(|result| result.match_kind == "exact"));
        assert_eq!(report.query.sha256, fingerprint.sha256);
        assert_eq!(report.query.phash, fingerprint.phash);
        assert_eq!(report.query.width, fingerprint.width);
        assert_eq!(report.query.height, fingerprint.height);
        assert!(report.unavailable.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn query_all_exact_short_circuit_works_for_undecodable_files() {
        let root = unique_temp_root("image-index-all-exact-no-decode");
        let account = "all_exact_no_decode";
        std::fs::create_dir_all(&root).unwrap();
        let query_path = root.join("not-an-image.jpg");
        let duplicate_path = root.join("duplicate.jpg");
        std::fs::write(&query_path, b"this is not decodable image data").unwrap();
        std::fs::copy(&query_path, &duplicate_path).unwrap();

        let manifest = default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let con = Connection::open(&manifest).unwrap();
        init_schema(&con).unwrap();
        let sha256 = sha256_file(&query_path).unwrap();
        for path in [&query_path, &duplicate_path] {
            con.execute(
                "insert into image_assets (path, sha256_hex, stale) values (?1, ?2, 0)",
                params![path.to_string_lossy().to_string(), sha256.clone()],
            )
            .unwrap();
        }
        drop(con);

        let report = query_image(QueryImageOptions {
            root: root.clone(),
            account: account.to_string(),
            manifest_path: None,
            image: query_path,
            mode: QueryMode::All,
            query_strategy: QueryStrategy::Exact,
            limit: 2,
            hamming_threshold: DEFAULT_HAMMING_THRESHOLD,
            model_dir: None,
            sscd_model_dir: None,
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "cpu".to_string(),
        })
        .unwrap();

        assert_eq!(report.results.len(), 2);
        assert!(report
            .results
            .iter()
            .all(|result| result.match_kind == "exact"));
        assert_eq!(report.query.sha256, sha256);
        assert_eq!(report.query.phash, "");
        assert_eq!(report.query.width, 0);
        assert_eq!(report.query.height, 0);
        assert!(report.unavailable.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn record_path_error_clears_lookup_data_and_old_embeddings() {
        let con = test_manifest().unwrap();
        insert_test_embedding(&con, "bad.png", "clip", "m", &[0.9, 0.0]).unwrap();
        con.execute("update image_assets set stale=1 where path='bad.png'", [])
            .unwrap();

        record_path_error(&con, "bad.png", "decode failed").unwrap();

        let (sha, phash, stale, error): (String, Option<String>, i64, String) = con
            .query_row(
                "select sha256_hex, phash_hex, stale, error from image_assets where path='bad.png'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(sha, "");
        assert_eq!(phash, None);
        assert_eq!(stale, 0);
        assert_eq!(error, "decode failed");
        assert_eq!(
            count_i64(
                &con,
                "select count(*) from image_embeddings where path='bad.png'"
            )
            .unwrap(),
            0
        );
        assert!(exact_matches(&con, "sha-bad.png", 10).unwrap().is_empty());
        assert!(
            semantic_matches(&con, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn record_item_error_keeps_hashes_but_removes_stale_embeddings() {
        let con = test_manifest().unwrap();
        insert_test_embedding(&con, "same.png", "clip", "m", &[0.9, 0.0]).unwrap();
        let item = test_pending_item("same.png", "sha-new", "0000000000000000");

        record_item_error(&con, &item, "embedding failed").unwrap();

        assert_eq!(
            count_i64(
                &con,
                "select count(*) from image_embeddings where path='same.png'"
            )
            .unwrap(),
            0
        );
        let exact = exact_matches(&con, "sha-new", 10).unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].path, PathBuf::from("same.png"));
        assert!(
            semantic_matches(&con, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 10)
                .unwrap()
                .is_empty()
        );
        let (stale, error): (i64, String) = con
            .query_row(
                "select stale, error from image_assets where path='same.png'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stale, 0);
        assert_eq!(error, "embedding failed");
    }

    #[test]
    fn semantic_matches_keep_only_top_k_with_stable_ties() {
        let con = test_manifest().unwrap();
        insert_test_embedding(&con, "z.png", "clip", "m", &[0.8, 0.0]).unwrap();
        insert_test_embedding(&con, "b.png", "clip", "m", &[0.9, 0.0]).unwrap();
        insert_test_embedding(&con, "a.png", "clip", "m", &[0.9, 0.0]).unwrap();

        let results =
            semantic_matches(&con, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 2).unwrap();
        let names = results
            .iter()
            .map(|(item, score)| {
                (
                    item.path.to_string_lossy().to_string(),
                    (*score * 1000.0).round() as i64,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![("a.png".to_string(), 900), ("b.png".to_string(), 900)]
        );
    }

    #[test]
    fn semantic_matches_skips_malformed_vector_blob() {
        let con = test_manifest().unwrap();
        insert_test_asset(&con, "bad.png", "0000000000000000").unwrap();
        con.execute(
            r#"
            insert into image_embeddings
                (path, kind, model, dim, vec, sketch64_hex, bucket12, updated_at)
            values
                ('bad.png', 'clip', 'm', 2, ?1, '', null, 'bad-time')
            "#,
            [vec![0u8; 4]],
        )
        .unwrap();
        insert_test_embedding(&con, "good.png", "clip", "m", &[0.9, 0.0]).unwrap();

        let results =
            semantic_matches(&con, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.path, PathBuf::from("good.png"));
    }

    #[test]
    fn semantic_matches_accepts_legacy_path_model_key() {
        let con = test_manifest().unwrap();
        insert_test_embedding(
            &con,
            "legacy.png",
            "clip",
            "m:relative/model/path",
            &[0.95, 0.0],
        )
        .unwrap();
        insert_test_embedding(&con, "current.png", "clip", "m", &[0.9, 0.0]).unwrap();

        let results =
            semantic_matches(&con, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 2).unwrap();
        let names = results
            .iter()
            .map(|(item, score)| {
                (
                    item.path.to_string_lossy().to_string(),
                    (*score * 1000.0).round() as i64,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("legacy.png".to_string(), 950),
                ("current.png".to_string(), 900)
            ]
        );
    }

    #[test]
    fn semantic_matches_deduplicates_current_and_legacy_rows_by_path() {
        let con = test_manifest().unwrap();
        insert_test_embedding(&con, "same.png", "clip", "m", &[0.8, 0.0]).unwrap();
        upsert_embedding(&con, "same.png", "clip", "m:old/model/path", &[0.95, 0.0]).unwrap();
        insert_test_embedding(&con, "other.png", "clip", "m", &[0.9, 0.0]).unwrap();

        let results =
            semantic_matches(&con, "clip", "m", &[1.0, 0.0], QueryStrategy::Exact, 10).unwrap();
        let names = results
            .iter()
            .map(|(item, score)| {
                (
                    item.path.to_string_lossy().to_string(),
                    (*score * 1000.0).round() as i64,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("same.png".to_string(), 950),
                ("other.png".to_string(), 900)
            ]
        );
    }

    #[test]
    fn exact_strategy_uses_full_scan_when_fast_shortlist_is_large_enough() {
        let con = test_manifest().unwrap();
        let query = [1.0, 0.0, 0.0, 0.0];
        let query_buckets = vector_probe_buckets(vector_bucket12(vector_sketch64(&query)));

        for idx in 0..300 {
            let score = (idx + 1) as f32 / 1000.0;
            insert_test_embedding(
                &con,
                &format!("item_{idx:03}.png"),
                "clip",
                "m",
                &[score, 0.0, 0.0, 0.0],
            )
            .unwrap();
        }

        let outside_vector = (0..10_000)
            .map(|idx| {
                let a = ((idx % 21) as f32 - 10.0) / 3.0;
                let b = (((idx / 21) % 21) as f32 - 10.0) / 3.0;
                let c = (((idx / 441) % 21) as f32 - 10.0) / 3.0;
                [0.95, a, b, c]
            })
            .find(|vector| {
                let bucket = vector_bucket12(vector_sketch64(vector));
                !query_buckets.contains(&bucket)
            })
            .expect("test should find an out-of-bucket high-scoring vector");
        insert_test_embedding(&con, "outside.png", "clip", "m", &outside_vector).unwrap();

        let exact = semantic_matches(&con, "clip", "m", &query, QueryStrategy::Exact, 2).unwrap();
        assert_eq!(exact[0].0.path, PathBuf::from("outside.png"));

        let fast = semantic_matches(&con, "clip", "m", &query, QueryStrategy::Fast, 2).unwrap();
        assert_ne!(fast[0].0.path, PathBuf::from("outside.png"));
    }

    #[test]
    fn semantic_bucket_shortlist_returns_top_k_when_candidate_pool_is_large_enough() {
        let con = test_manifest().unwrap();
        for idx in 0..300 {
            let score = (idx + 1) as f32 / 1000.0;
            insert_test_embedding(
                &con,
                &format!("item_{idx:03}.png"),
                "clip",
                "m",
                &[score, 0.0],
            )
            .unwrap();
        }

        let scan = semantic_bucket_shortlist(&con, "clip", "m", &[1.0, 0.0], 2).unwrap();
        let results = sort_semantic_results(scan.results);
        let names = results
            .iter()
            .map(|(item, _)| item.path.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["item_299.png".to_string(), "item_298.png".to_string()]
        );
    }

    #[test]
    fn push_top_semantic_keeps_sorted_top_k_after_duplicate_replacement() {
        let mut results = Vec::new();
        push_top_semantic(
            &mut results,
            (test_indexed_image("b.png", "sha-b"), 0.80),
            3,
        );
        push_top_semantic(
            &mut results,
            (test_indexed_image("a.png", "sha-a"), 0.90),
            3,
        );
        push_top_semantic(
            &mut results,
            (test_indexed_image("c.png", "sha-c"), 0.70),
            3,
        );
        push_top_semantic(
            &mut results,
            (test_indexed_image("d.png", "sha-d"), 0.10),
            3,
        );
        push_top_semantic(
            &mut results,
            (test_indexed_image("b.png", "sha-b-new"), 0.95),
            3,
        );

        let names = results
            .iter()
            .map(|(item, score)| {
                (
                    item.path.to_string_lossy().to_string(),
                    (*score * 100.0).round() as i64,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("b.png".to_string(), 95),
                ("a.png".to_string(), 90),
                ("c.png".to_string(), 70),
            ]
        );
        assert_eq!(results[0].0.sha256, "sha-b-new");
    }

    #[test]
    fn near_hash_matches_keep_only_top_k_with_stable_ties() {
        let con = test_manifest().unwrap();
        insert_test_asset(&con, "d.png", "0000000000000003").unwrap();
        insert_test_asset(&con, "c.png", "0000000000000001").unwrap();
        insert_test_asset(&con, "a.png", "0000000000000001").unwrap();
        insert_test_asset(&con, "b.png", "0000000000000000").unwrap();
        insert_test_asset(&con, "invalid.png", "not-a-hex-phash").unwrap();

        let results = near_hash_matches(&con, "0000000000000000", PHASH_ALGO_V2, 2, 3).unwrap();
        let names = results
            .iter()
            .map(|(item, distance)| (item.path.to_string_lossy().to_string(), *distance))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("b.png".to_string(), 0),
                ("a.png".to_string(), 1),
                ("c.png".to_string(), 1),
            ]
        );
    }

    #[test]
    fn push_top_near_hash_keeps_sorted_top_k() {
        let mut results = Vec::new();
        push_top_near_hash(&mut results, (test_indexed_image("c.png", "sha-c"), 2), 3);
        push_top_near_hash(&mut results, (test_indexed_image("a.png", "sha-a"), 1), 3);
        push_top_near_hash(&mut results, (test_indexed_image("b.png", "sha-b"), 1), 3);
        push_top_near_hash(&mut results, (test_indexed_image("d.png", "sha-d"), 4), 3);
        push_top_near_hash(&mut results, (test_indexed_image("e.png", "sha-e"), 0), 3);

        let names = results
            .iter()
            .map(|(item, distance)| (item.path.to_string_lossy().to_string(), *distance))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("e.png".to_string(), 0),
                ("a.png".to_string(), 1),
                ("b.png".to_string(), 1),
            ]
        );
    }

    #[test]
    fn push_result_keeps_stronger_signal_before_raw_score() {
        let mut results = Vec::new();
        let item = test_indexed_image("same.png", "sha");
        push_result(&mut results, "semantic_clip", 0.99, None, item.clone(), 10);
        push_result(&mut results, "copy_sscd", 0.70, None, item.clone(), 10);
        assert_eq!(results[0].match_kind, "copy_sscd");
        assert_eq!(results[0].score, 0.70);

        push_result(&mut results, "near_hash", 0.95, Some(3), item.clone(), 10);
        assert_eq!(results[0].match_kind, "copy_sscd");

        push_result(&mut results, "exact", 1.0, None, item, 10);
        assert_eq!(results[0].match_kind, "exact");
    }

    #[test]
    fn push_result_bounds_all_match_kinds_by_evidence_priority() {
        let mut results = Vec::new();
        push_result(
            &mut results,
            "semantic_clip",
            0.99,
            None,
            test_indexed_image("semantic.png", "sha-semantic"),
            2,
        );
        push_result(
            &mut results,
            "copy_sscd",
            0.70,
            None,
            test_indexed_image("copy.png", "sha-copy"),
            2,
        );
        push_result(
            &mut results,
            "near_hash",
            0.95,
            Some(3),
            test_indexed_image("near.png", "sha-near"),
            2,
        );

        results.sort_by(query_result_sort_cmp);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "copy.png");
        assert_eq!(results[1].path, "near.png");

        push_result(
            &mut results,
            "exact",
            1.0,
            None,
            test_indexed_image("exact.png", "sha-exact"),
            2,
        );
        results.sort_by(query_result_sort_cmp);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "exact.png");
        assert_eq!(results[1].path, "copy.png");
    }

    #[test]
    fn push_result_keeps_sorted_bounded_buffer() {
        let mut results = Vec::new();
        push_result(
            &mut results,
            "semantic_clip",
            0.99,
            None,
            test_indexed_image("semantic.png", "sha-semantic"),
            2,
        );
        push_result(
            &mut results,
            "copy_sscd",
            0.70,
            None,
            test_indexed_image("copy.png", "sha-copy"),
            2,
        );
        push_result(
            &mut results,
            "near_hash",
            0.95,
            Some(3),
            test_indexed_image("near.png", "sha-near"),
            2,
        );

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "copy.png");
        assert_eq!(results[1].path, "near.png");

        push_result(
            &mut results,
            "exact",
            1.0,
            None,
            test_indexed_image("exact.png", "sha-exact"),
            2,
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "exact.png");
        assert_eq!(results[1].path, "copy.png");
    }

    #[test]
    fn result_buffer_excludes_only_lower_priority_match_kinds() {
        let mut exact_results = Vec::new();
        push_result(
            &mut exact_results,
            "exact",
            1.0,
            None,
            test_indexed_image("a.png", "sha-a"),
            2,
        );
        push_result(
            &mut exact_results,
            "exact",
            1.0,
            None,
            test_indexed_image("b.png", "sha-b"),
            2,
        );
        assert!(result_buffer_excludes_kind(&exact_results, 2, "copy_sscd"));
        assert!(result_buffer_excludes_kind(&exact_results, 2, "near_hash"));
        assert!(result_buffer_excludes_kind(
            &exact_results,
            2,
            "semantic_clip"
        ));
        assert!(!result_buffer_excludes_kind(&exact_results, 2, "exact"));

        let mut mixed_results = Vec::new();
        push_result(
            &mut mixed_results,
            "copy_sscd",
            0.70,
            None,
            test_indexed_image("copy.png", "sha-copy"),
            2,
        );
        push_result(
            &mut mixed_results,
            "near_hash",
            0.95,
            Some(3),
            test_indexed_image("near.png", "sha-near"),
            2,
        );
        assert!(!result_buffer_excludes_kind(&mixed_results, 2, "copy_sscd"));
        assert!(!result_buffer_excludes_kind(&mixed_results, 2, "near_hash"));
        assert!(result_buffer_excludes_kind(
            &mixed_results,
            2,
            "semantic_clip"
        ));
    }

    #[test]
    fn status_reports_embedding_sketch_coverage() {
        let root = unique_temp_root("image-index-status");
        let account = "status_account";
        let manifest = default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let con = Connection::open(&manifest).unwrap();
        init_schema(&con).unwrap();
        insert_test_asset(&con, "a.png", "0000000000000000").unwrap();
        upsert_embedding(&con, "a.png", "clip", "clip-model", &[1.0, 0.0]).unwrap();
        upsert_embedding(&con, "a.png", "sscd", "sscd-model", &[1.0, 0.0]).unwrap();
        drop(con);

        let report = status(&root, account).unwrap();
        assert!(report.manifest_exists);
        assert_eq!(report.images, 1);
        assert_eq!(report.embeddings.rows, 2);
        assert_eq!(report.embeddings.active_rows, 2);
        assert_eq!(report.embeddings.active_clip_rows, 1);
        assert_eq!(report.embeddings.active_sscd_rows, 1);
        assert_eq!(report.embeddings.active_rows_with_sketch, 2);
        assert_eq!(report.embeddings.active_rows_missing_sketch, 0);
        assert!(report.embeddings.active_buckets >= 1);

        let _ = std::fs::remove_dir_all(root);
    }

    fn test_manifest() -> anyhow::Result<Connection> {
        let con = Connection::open_in_memory()?;
        init_schema(&con)?;
        Ok(con)
    }

    fn table_columns(con: &Connection, table: &str) -> Vec<String> {
        let mut stmt = con.prepare(&format!("pragma table_info({table})")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn insert_test_embedding(
        con: &Connection,
        path: &str,
        kind: &str,
        model: &str,
        vector: &[f32],
    ) -> anyhow::Result<()> {
        insert_test_asset(con, path, "0000000000000000")?;
        upsert_embedding(con, path, kind, model, vector)
    }

    fn insert_raw_embedding(
        con: &Connection,
        path: &str,
        kind: &str,
        model: &str,
        vector: &[f32],
        updated_at: &str,
    ) -> anyhow::Result<()> {
        let sketch = vector_sketch64(vector);
        con.execute(
            r#"
            insert into image_embeddings
                (path, kind, model, dim, vec, sketch64_hex, bucket12, updated_at)
            values
                (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                path,
                kind,
                model,
                vector.len() as i64,
                encode_f32_vec(vector),
                format!("{sketch:016x}"),
                vector_bucket12(sketch) as i64,
                updated_at,
            ],
        )?;
        Ok(())
    }

    fn insert_test_asset(con: &Connection, path: &str, phash: &str) -> anyhow::Result<()> {
        con.execute(
            "insert into image_assets (path, sha256_hex, phash_hex, phash_algo, width, height, stale) values (?1, ?2, ?3, ?4, 1, 1, 0)",
            params![path, format!("sha-{path}"), phash, PHASH_ALGO_V2],
        )?;
        Ok(())
    }

    fn models_for_path(con: &Connection, path: &str) -> Vec<String> {
        con.prepare("select model from image_embeddings where path=?1 order by model")
            .unwrap()
            .query_map([path], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn active_asset_paths(con: &Connection) -> Vec<String> {
        con.prepare("select path from image_assets where stale=0 order by path")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn test_build_options(
        root: &Path,
        account: &str,
        asset_root: &Path,
        max_files: usize,
    ) -> BuildOptions {
        BuildOptions {
            root: root.to_path_buf(),
            account: account.to_string(),
            asset_roots: vec![asset_root.to_path_buf()],
            manifest_path: None,
            max_files,
            force: false,
            pipeline: "full".to_string(),
            stage: BuildStage::All,
            model_dir: None,
            sscd_model_dir: None,
            clip_model: "mobileclip2-s2".to_string(),
            sscd_model: "sscd_disc_mixup".to_string(),
            execution_provider: "cpu".to_string(),
            backend: "manifest-sqlite".to_string(),
            clip_batch_size: 8,
            manifest_workers: 1,
            manifest_mode: ManifestMode::Full,
        }
    }

    fn write_test_png(path: &Path, rgb: [u8; 3]) {
        image::RgbImage::from_pixel(8, 8, image::Rgb(rgb))
            .save(path)
            .unwrap();
    }

    fn write_test_jpeg(path: &Path) {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(16, 16, |x, y| {
            image::Rgb([
                ((x * 17 + y * 3) % 256) as u8,
                ((x * 5 + y * 11) % 256) as u8,
                ((x * 13 + y * 7) % 256) as u8,
            ])
        }));
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85);
        encoder.encode_image(&image).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn write_patch_fixture(base_path: &Path, crop_path: &Path) {
        let image = image::RgbImage::from_fn(64, 64, |x, y| {
            if x < 32 && y < 32 {
                image::Rgb([
                    ((x * 7 + y * 13) % 256) as u8,
                    ((x * 17 + y * 3) % 256) as u8,
                    ((x * 5 + y * 19) % 256) as u8,
                ])
            } else if x >= 32 && y < 32 {
                image::Rgb([220, ((x + y) % 64) as u8, 40])
            } else if x < 32 {
                image::Rgb([30, 180, ((x * 2 + y) % 96) as u8])
            } else {
                image::Rgb([90, 40, 210])
            }
        });
        image.save(base_path).unwrap();
        let crop = image::imageops::crop_imm(&image, 0, 0, 32, 32).to_image();
        crop.save(crop_path).unwrap();
    }

    fn test_pending_item(path: &str, sha256: &str, phash: &str) -> PendingIndexImage {
        PendingIndexImage {
            source_root: PathBuf::from("."),
            path: PathBuf::from(path),
            path_text: path.to_string(),
            file_size: 1,
            mtime_unix: 1,
            fingerprint: ImageFingerprint {
                sha256: sha256.to_string(),
                phash: phash.to_string(),
                phash_algo: PHASH_ALGO_V2.to_string(),
                width: 1,
                height: 1,
                blur_score: 0.0,
                blur_algo: BLUR_ALGO_V2.to_string(),
                quality_flags: "small,tiny".to_string(),
                source_class: "image".to_string(),
                detected_format: "TEST".to_string(),
                has_alpha: false,
                orientation_applied: false,
                fingerprint_version: FINGERPRINT_VERSION_V2.to_string(),
                tile_hashes: Vec::new(),
            },
            image: None,
            reuse_existing_asset: false,
        }
    }

    fn test_indexed_image(path: &str, sha256: &str) -> IndexedImage {
        IndexedImage {
            path: PathBuf::from(path),
            sha256: sha256.to_string(),
            phash: Some("0000000000000000".to_string()),
            phash_algo: Some(PHASH_ALGO_V2.to_string()),
            width: Some(1),
            height: Some(1),
            blur_score: Some(0.0),
            blur_algo: Some(BLUR_ALGO_V2.to_string()),
            quality_flags: String::new(),
            source_class: "image".to_string(),
            tile_algo: None,
            matched_tile_count: None,
        }
    }

    #[test]
    fn legacy_exact_groups_are_normalized_during_manifest_refresh() {
        let mut con = Connection::open_in_memory().unwrap();
        con.execute_batch(
            r#"create table image_exact_groups (
                   asset_id integer primary key,
                   group_key text not null,
                   representative_asset_id integer not null,
                   member_count integer not null,
                   total_bytes integer not null,
                   duplicate_bytes integer not null
               );
               insert into image_exact_groups values(1,'sha256:same',2,2,30,10);
               insert into image_exact_groups values(2,'sha256:same',2,2,30,10);
               insert into image_exact_groups values(3,'asset:3',3,1,7,0);"#,
        )
        .unwrap();

        init_schema(&con).unwrap();
        init_schema(&con).unwrap();

        let startup_object_type: String = con
            .query_row(
                "select type from sqlite_master where name='image_exact_groups'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(startup_object_type, "table");
        con.execute_batch(
            r#"insert into image_assets(
                   id,path,file_size,sha256_hex,width,height
               ) values(1,'one.png',10,'same',1,1);
               insert into image_assets(
                   id,path,file_size,sha256_hex,width,height
               ) values(2,'two.png',20,'same',2,2);
               insert into image_assets(
                   id,path,file_size,sha256_hex,width,height
               ) values(3,'three.png',7,'',1,1);"#,
        )
        .unwrap();
        refresh_exact_groups(&mut con).unwrap();

        let object_type: String = con
            .query_row(
                "select type from sqlite_master where name='image_exact_groups'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(object_type, "view");
        let counts: (i64, i64, i64) = con
            .query_row(
                r#"select
                       (select count(*) from image_exact_group_members),
                       (select count(*) from image_exact_group_stats),
                       (select count(*) from image_exact_groups)"#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (3, 2, 3));
        let migrated: (String, i64, i64, i64) = con
            .query_row(
                r#"select group_key,representative_asset_id,member_count,
                          duplicate_bytes
                   from image_exact_groups where asset_id=1"#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(migrated, ("sha256:same".to_string(), 2, 2, 10));
    }

    #[test]
    fn exact_group_representative_prefers_health_quality_area_size_then_id() {
        let mut con = Connection::open_in_memory().unwrap();
        init_schema(&con).unwrap();
        for (path, sha, width, height, size, flags, error) in [
            ("bad.png", "same", 100, 100, 500, "", Some("decode")),
            ("blurry.png", "same", 200, 200, 900, "blurry", None),
            ("clean-small.png", "same", 20, 20, 50, "", None),
            ("clean-large.png", "same", 20, 20, 70, "", None),
            ("no-hash.png", "", 1, 1, 1, "", None),
        ] {
            con.execute(
                "insert into image_assets(path,sha256_hex,width,height,file_size,quality_flags,error) values(?1,?2,?3,?4,?5,?6,?7)",
                params![path, sha, width, height, size, flags, error],
            )
            .unwrap();
        }
        refresh_exact_groups(&mut con).unwrap();
        let representative: i64 = con
            .query_row(
                "select representative_asset_id from image_exact_groups where asset_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(representative, 4);
        let no_hash: (i64, i64) = con
            .query_row(
                "select representative_asset_id,member_count from image_exact_groups where asset_id=5",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(no_hash, (5, 1));

        con.execute(
            "insert into image_embeddings(path,kind,model,dim,vec) values('clean-large.png','clip','test',1,x'00000000')",
            [],
        )
        .unwrap();
        refresh_overview_cache(&mut con).unwrap();
        let cached: String = con
            .query_row(
                "select payload_json from image_index_overview_cache where id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let cached = serde_json::from_str::<Value>(&cached).unwrap();
        assert_eq!(cached["health"]["assets"], 5);
        assert_eq!(cached["coverage"]["clip_assets"], 1);
        assert_eq!(cached["exact_copies"]["groups"], 2);
        assert_eq!(cached["exact_copies"]["duplicate_files"], 3);

        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(4,'c2c_test',7)",
            [],
        )
        .unwrap();
        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(1,'c2c_test',8)",
            [],
        )
        .unwrap();
        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(1,'c2c_test',7)",
            [],
        )
        .unwrap();
        refresh_overview_reference_cache(&mut con).unwrap();
        let cached: String = con
            .query_row(
                "select payload_json from image_index_overview_cache where id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let cached = serde_json::from_str::<Value>(&cached).unwrap();
        assert_eq!(cached["coverage"]["occurrences"], 3);
        assert_eq!(cached["coverage"]["referenced_assets"], 2);
        let direct_count: i64 = con
            .query_row(
                "select reference_count from image_asset_reference_counts where asset_id=4",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(direct_count, 1);
        let exact_count: i64 = con
            .query_row(
                "select reference_count from image_exact_reference_counts where representative_asset_id=4",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exact_count, 2);
        let cached_occurrences: i64 = con
            .query_row(
                "select occurrence_row_count from image_popularity_cache_state where id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cached_occurrences, 3);
    }

    #[test]
    fn popularity_cache_upgrade_rebuilds_asset_counts_and_reuses_completed_cube() {
        let mut con = Connection::open_in_memory().unwrap();
        init_schema(&con).unwrap();
        con.execute(
            "insert into image_assets(id,path,sha256_hex,file_size) values(1,'a.png','a',10)",
            [],
        )
        .unwrap();
        refresh_exact_groups(&mut con).unwrap();
        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(1,'group_1',1)",
            [],
        )
        .unwrap();
        refresh_popularity_cache(&mut con, 1).unwrap();

        con.execute(
            "update image_asset_reference_counts set reference_count=42 where asset_id=1",
            [],
        )
        .unwrap();
        con.execute(
            r#"insert into image_popularity_cube(
                   representative_asset_id,message_date,sender_uin,conversation_table,
                   conversation_type,reference_count
               ) values(1,'2024-01-01','sender','group_1','group',5)"#,
            [],
        )
        .unwrap();
        con.execute(
            r#"insert into image_popularity_build_state(
                   id,schema_version,occurrence_count,fact_count,cube_row_count,
                   summary_ready,refreshed_at
               ) values(1,1,1,1,1,1,'now')"#,
            [],
        )
        .unwrap();
        con.execute(
            "update image_popularity_cache_state set schema_version=1 where id=1",
            [],
        )
        .unwrap();

        refresh_popularity_cache(&mut con, 1).unwrap();

        let asset_count: i64 = con
            .query_row(
                "select reference_count from image_asset_reference_counts where asset_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(asset_count, 1);
        let exact_count: i64 = con
            .query_row(
                "select reference_count from image_exact_reference_counts where representative_asset_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exact_count, 5);
        let state: (i64, i64) = con
            .query_row(
                "select schema_version,occurrence_row_count from image_popularity_cache_state where id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, (POPULARITY_CACHE_SCHEMA_VERSION, 1));
    }

    #[test]
    fn popularity_cache_rebuilds_when_occurrence_members_change_at_same_count() {
        let mut con = Connection::open_in_memory().unwrap();
        init_schema(&con).unwrap();
        con.execute_batch(
            r#"insert into image_assets(id,path,sha256_hex,file_size)
                   values(1,'a.png','a',10);
               insert into image_assets(id,path,sha256_hex,file_size)
                   values(2,'b.png','b',10);"#,
        )
        .unwrap();
        refresh_exact_groups(&mut con).unwrap();
        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(1,'group_1',1)",
            [],
        )
        .unwrap();
        mark_occurrence_content_changed(&con).unwrap();
        refresh_popularity_cache(&mut con, 1).unwrap();

        con.execute("delete from image_asset_occurrences", [])
            .unwrap();
        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(2,'group_1',1)",
            [],
        )
        .unwrap();
        mark_occurrence_content_changed(&con).unwrap();
        refresh_popularity_cache(&mut con, 1).unwrap();

        let counts = con
            .prepare(
                "select asset_id,reference_count from image_asset_reference_counts order by asset_id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(counts, vec![(2, 1)]);
    }

    #[test]
    fn web_prepare_detects_occurrence_generation_change_at_same_count() {
        let root = std::env::temp_dir().join(format!(
            "qq-analyzer-popularity-generation-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let manifest = root.join("manifest.sqlite");
        std::fs::create_dir_all(&root).unwrap();
        drop(Connection::open(&manifest).unwrap());
        prepare_web_manifest_path(&manifest).unwrap();
        let mut con = Connection::open(&manifest).unwrap();
        con.execute_batch(
            r#"insert into image_assets(id,path,sha256_hex,file_size)
                   values(1,'a.png','a',10);
               insert into image_assets(id,path,sha256_hex,file_size)
                   values(2,'b.png','b',10);"#,
        )
        .unwrap();
        refresh_exact_groups(&mut con).unwrap();
        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(1,'group_1',1)",
            [],
        )
        .unwrap();
        mark_occurrence_content_changed(&con).unwrap();
        refresh_overview_cache(&mut con).unwrap();
        refresh_overview_reference_cache(&mut con).unwrap();

        con.execute("delete from image_asset_occurrences", [])
            .unwrap();
        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(2,'group_1',1)",
            [],
        )
        .unwrap();
        mark_occurrence_content_changed(&con).unwrap();
        drop(con);

        prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        let counts = con
            .prepare(
                "select asset_id,reference_count from image_asset_reference_counts order by asset_id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(counts, vec![(2, 1)]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn exact_group_refresh_preserves_popularity_until_representative_mapping_changes() {
        let mut con = Connection::open_in_memory().unwrap();
        init_schema(&con).unwrap();
        con.execute(
            "insert into image_assets(path,sha256_hex,file_size) values('a.png','a',10)",
            [],
        )
        .unwrap();
        con.execute(
            "insert into image_assets(path,sha256_hex,file_size) values('b.png','b',20)",
            [],
        )
        .unwrap();
        refresh_exact_groups(&mut con).unwrap();
        con.execute(
            "insert into image_exact_reference_counts(representative_asset_id,reference_count) values(1,7)",
            [],
        )
        .unwrap();

        refresh_exact_groups(&mut con).unwrap();
        let preserved: i64 = con
            .query_row(
                "select reference_count from image_exact_reference_counts where representative_asset_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved, 7);

        con.execute("update image_assets set sha256_hex='a' where id=2", [])
            .unwrap();
        refresh_exact_groups(&mut con).unwrap();
        let invalidated: i64 = con
            .query_row(
                "select count(*) from image_exact_reference_counts",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invalidated, 0);
    }

    fn unique_temp_root(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn vector_sketch64_uncached_for_test(vector: &[f32]) -> u64 {
        let mut sketch = 0u64;
        for bit in 0..VECTOR_SKETCH_BITS {
            let mut acc = 0.0f32;
            for (idx, value) in vector.iter().enumerate() {
                let sign = if sketch_projection_positive(idx, bit) {
                    1.0
                } else {
                    -1.0
                };
                acc += *value * sign;
            }
            if acc >= 0.0 {
                sketch |= 1u64 << bit;
            }
        }
        sketch
    }
}

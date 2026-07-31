use std::collections::HashMap;
#[cfg(feature = "image-index")]
use std::collections::HashSet;
use std::io::SeekFrom;
use std::net::IpAddr;
use std::path::Path;
#[cfg(feature = "image-index")]
use std::path::PathBuf;
#[cfg(feature = "image-index")]
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(feature = "image-index")]
use std::time::Duration;
use std::time::UNIX_EPOCH;

use axum::body::Body;
#[cfg(feature = "image-index")]
use axum::extract::Multipart;
use axum::extract::{DefaultBodyLimit, Path as RoutePath, Query, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH,
    LAST_MODIFIED, RANGE,
};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
#[cfg(feature = "image-index")]
use tokio::sync::{mpsc, oneshot};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use msg3_richtext_parser_rs::catalog;
use msg3_richtext_parser_rs::info_storage;
use msg3_richtext_parser_rs::msg3_log_service_asset_http::{
    content_type_for_path, decode_asset_path,
};
use msg3_richtext_parser_rs::msg3_log_service_config::{parse_args, Config};
#[cfg(feature = "image-index")]
use msg3_richtext_parser_rs::msg3_log_service_conversations::enrich_insights_overview_labels;
use msg3_richtext_parser_rs::msg3_log_service_conversations::{
    conversation_detail_json, conversation_details_json, conversations_json,
    preload_conversation_cache,
};
#[cfg(feature = "image-index")]
use msg3_richtext_parser_rs::msg3_log_service_messages::image_reference_analysis_json;
use msg3_richtext_parser_rs::msg3_log_service_messages::{message_detail_json, messages_json};
use msg3_richtext_parser_rs::msg3_log_service_models::{Conversation, ConversationLight};
#[cfg(feature = "image-index")]
use msg3_richtext_parser_rs::{image_index, image_index_link, image_index_web, image_insights};

#[cfg(feature = "web-ui")]
use rust_embed::RustEmbed;

const CHAT_POOL_SIZE: usize = 4;
#[cfg(feature = "image-index")]
const SEARCH_QUEUE_CAPACITY: usize = 8;
#[cfg(feature = "image-index")]
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5 * 60);
#[cfg(feature = "image-index")]
const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;
#[cfg(feature = "image-index")]
const THUMBNAIL_WORKERS: usize = 4;
#[cfg(feature = "image-index")]
const ASSET_PATH_CACHE_CAPACITY: usize = 4_096;

#[cfg(feature = "web-ui")]
#[derive(RustEmbed)]
#[folder = "../web/dist"]
struct WebAssets;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    chat: ChatDbPool,
    info: info_storage::InfoStorage,
    friend_social: info_storage::InfoStorage,
    caches: Arc<Mutex<ChatCaches>>,
    #[cfg(feature = "image-index")]
    thumbnail_workers: Arc<Semaphore>,
    #[cfg(feature = "image-index")]
    search: SearchActor,
    #[cfg(feature = "image-index")]
    maintenance: MaintenanceController,
    #[cfg(feature = "image-index")]
    popularity_maintenance: MaintenanceController,
    #[cfg(feature = "image-index")]
    same_image_cache: Arc<image_insights::SameImageCache>,
    #[cfg(feature = "image-index")]
    ranking_tasks: image_insights::RankingTaskCache,
    #[cfg(feature = "image-index")]
    insights_overview_cache: Arc<Mutex<image_insights::InsightsOverviewCache>>,
    #[cfg(feature = "image-index")]
    insights_compare_cache: Arc<Mutex<image_insights::InsightsOverviewCache>>,
    #[cfg(feature = "image-index")]
    asset_path_cache: Arc<Mutex<HashMap<i64, PathBuf>>>,
}

#[derive(Default)]
struct ChatCaches {
    light: HashMap<String, ConversationLight>,
    detail: HashMap<String, Conversation>,
}

#[derive(Clone)]
struct ChatDbPool {
    connections: Arc<Mutex<Vec<Connection>>>,
    available: Arc<Semaphore>,
}

impl ChatDbPool {
    fn open(path: &Path, size: usize) -> anyhow::Result<Self> {
        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            connections.push(Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?);
        }
        Ok(Self {
            connections: Arc::new(Mutex::new(connections)),
            available: Arc::new(Semaphore::new(size)),
        })
    }

    async fn run<T, F>(&self, operation: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let permit = self
            .available
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApiError::unavailable("chat database pool is closed"))?;
        let connections = self.connections.clone();
        tokio::task::spawn_blocking(move || run_with_connection(connections, permit, operation))
            .await
            .map_err(|err| ApiError::internal(format!("chat worker failed: {err}")))?
            .map_err(ApiError::from)
    }
}

fn run_with_connection<T, F>(
    connections: Arc<Mutex<Vec<Connection>>>,
    _permit: OwnedSemaphorePermit,
    operation: F,
) -> anyhow::Result<T>
where
    F: FnOnce(&Connection) -> anyhow::Result<T>,
{
    let connection = connections
        .lock()
        .map_err(|_| anyhow::anyhow!("chat database pool lock was poisoned"))?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("chat database pool invariant failed"))?;
    let result = operation(&connection);
    connections
        .lock()
        .map_err(|_| anyhow::anyhow!("chat database pool lock was poisoned"))?
        .push(connection);
    result
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    #[cfg(feature = "image-index")]
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            message,
        )
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            [(CACHE_CONTROL, "no-store")],
            Json(json!({
                "error": {"code": self.code, "message": self.message}
            })),
        )
            .into_response()
    }
}

pub fn run() -> anyhow::Result<()> {
    let cfg = parse_args()?;
    validate_loopback_host(&cfg.host)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("qq-analyzer-web")
        .build()?;
    runtime.block_on(run_async(cfg))
}

async fn run_async(cfg: Config) -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "msg3_richtext_parser_rs=info,tower_http=info".into()),
        )
        .try_init();
    #[cfg(feature = "image-index")]
    image_index::prepare_web_manifest(&cfg.root, &cfg.account)?;
    let conversation_cache_path =
        msg3_richtext_parser_rs::config::account_output_root(&cfg.root, &cfg.account)
            .join("web-cache")
            .join("conversations-v1.json");
    let light_cache = preload_conversation_cache(&cfg.db, &conversation_cache_path)?;
    let chat = ChatDbPool::open(&cfg.db, CHAT_POOL_SIZE)?;
    let info = info_storage::InfoStorage::new(cfg.info_root.clone(), cfg.info_key_log.clone());
    let friend_social =
        info_storage::InfoStorage::new(cfg.friend_social_root.clone(), cfg.info_key_log.clone());
    #[cfg(feature = "image-index")]
    let search = SearchActor::start(cfg.root.clone(), cfg.account.clone());
    #[cfg(feature = "image-index")]
    let maintenance = MaintenanceController::default();
    #[cfg(feature = "image-index")]
    let popularity_maintenance = MaintenanceController::default();
    #[cfg(feature = "image-index")]
    let same_image_cache = Arc::new(image_insights::SameImageCache::default());
    #[cfg(feature = "image-index-qdrant")]
    {
        let vector_cache = same_image_cache.clone();
        let vector_root = cfg.root.clone();
        let vector_account = cfg.account.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = vector_cache.prepare_vector_index(&vector_root, &vector_account) {
                tracing::warn!(%error, "SSCD Qdrant vector index preparation failed");
            }
        });
    }
    #[cfg(feature = "image-index")]
    let ranking_tasks = image_insights::RankingTaskCache::new(same_image_cache.clone());
    let state = AppState {
        cfg: Arc::new(cfg.clone()),
        chat,
        info,
        friend_social,
        caches: Arc::new(Mutex::new(ChatCaches {
            light: light_cache,
            detail: HashMap::new(),
        })),
        #[cfg(feature = "image-index")]
        thumbnail_workers: Arc::new(Semaphore::new(THUMBNAIL_WORKERS)),
        #[cfg(feature = "image-index")]
        search,
        #[cfg(feature = "image-index")]
        maintenance,
        #[cfg(feature = "image-index")]
        popularity_maintenance,
        #[cfg(feature = "image-index")]
        same_image_cache,
        #[cfg(feature = "image-index")]
        ranking_tasks,
        #[cfg(feature = "image-index")]
        insights_overview_cache: Arc::new(Mutex::new(
            image_insights::InsightsOverviewCache::default(),
        )),
        #[cfg(feature = "image-index")]
        insights_compare_cache: Arc::new(Mutex::new(
            image_insights::InsightsOverviewCache::default(),
        )),
        #[cfg(feature = "image-index")]
        asset_path_cache: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
    println!("QQ Analyzer http://{}:{}/", cfg.host, cfg.port);
    println!("account={}", cfg.account);
    println!("db={}", cfg.db.display());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/status", get(api_status))
        .route("/api/maintenance/tasks", get(maintenance_tasks))
        .route(
            "/api/maintenance/tasks/{task_id}/start",
            post(maintenance_task_start),
        )
        .route(
            "/api/maintenance/tasks/{task_id}/pause",
            post(maintenance_task_pause),
        )
        .route("/api/conversations", get(api_conversations))
        .route("/api/conversation_detail", get(api_conversation_detail))
        .route("/api/conversation_details", get(api_conversation_details))
        .route("/api/messages", get(api_messages))
        .route("/api/message_detail", get(api_message_detail))
        .route("/api/image_index/status", get(legacy_image_status))
        .route("/api/search_images", get(legacy_text_search))
        .route("/api/similar_images", get(legacy_similar_search))
        .route("/asset/{encoded}", get(legacy_asset))
        .route("/api/image-index/overview", get(image_overview))
        .route(
            "/api/image-index/insights/overview",
            get(image_insights_overview),
        )
        .route(
            "/api/image-index/insights/assets",
            get(image_insights_assets),
        )
        .route(
            "/api/image-index/insights/compare",
            get(image_insights_compare),
        )
        .route("/api/image-index/assets", get(image_assets))
        .route("/api/image-index/assets/{id}", get(image_asset_detail))
        .route(
            "/api/image-index/assets/{id}/occurrences",
            get(image_asset_occurrences),
        )
        .route(
            "/api/image-index/assets/{id}/reference-analysis",
            get(image_asset_reference_analysis),
        )
        .route(
            "/api/image-index/assets/{id}/similar",
            get(image_asset_similar),
        )
        .route(
            "/api/image-index/assets/{id}/content",
            get(image_asset_content),
        )
        .route(
            "/api/image-index/assets/{id}/thumbnail",
            get(image_asset_thumbnail),
        )
        .route("/api/image-index/search/text", get(image_text_search))
        .route(
            "/api/image-index/search/image",
            post(image_upload_search).layer(DefaultBodyLimit::disable()),
        )
        .fallback(spa_fallback)
        .with_state(state)
        .layer(CatchPanicLayer::new())
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
}

fn validate_loopback_host(host: &str) -> anyhow::Result<()> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(());
    }
    let address = host
        .parse::<IpAddr>()
        .map_err(|_| anyhow::anyhow!("host must be a loopback IP address or localhost"))?;
    if !address.is_loopback() {
        anyhow::bail!(
            "non-loopback bind {host} is disabled; remote-access mode is not implemented"
        );
    }
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install Ctrl+C handler");
    }
}

fn json_body(body: String) -> Result<Response, ApiError> {
    let value = serde_json::from_str::<Value>(&body)
        .map_err(|error| ApiError::internal(format!("invalid internal JSON response: {error}")))?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

async fn api_status(State(state): State<AppState>) -> Result<Response, ApiError> {
    let cfg = state.cfg.clone();
    let info = state.info.clone();
    let body = state
        .chat
        .run(move |connection| status_json(&cfg, connection, &info))
        .await?;
    json_body(body)
}

async fn api_conversations(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let cfg = state.cfg.clone();
    let info = state.info.clone();
    let caches = state.caches.clone();
    let body = state
        .chat
        .run(move |connection| {
            let mut caches = caches
                .lock()
                .map_err(|_| anyhow::anyhow!("chat cache lock was poisoned"))?;
            conversations_json(
                &cfg.root,
                &cfg.account,
                connection,
                &info,
                &mut caches.light,
                &query,
            )
        })
        .await?;
    json_body(body)
}

async fn api_conversation_detail(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let cfg = state.cfg.clone();
    let info = state.info.clone();
    let caches = state.caches.clone();
    let body = state
        .chat
        .run(move |connection| {
            let mut caches = caches
                .lock()
                .map_err(|_| anyhow::anyhow!("chat cache lock was poisoned"))?;
            conversation_detail_json(&cfg, connection, &info, &mut caches.detail, &query)
        })
        .await?;
    json_body(body)
}

async fn api_conversation_details(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let cfg = state.cfg.clone();
    let info = state.info.clone();
    let caches = state.caches.clone();
    let body = state
        .chat
        .run(move |connection| {
            let mut caches = caches
                .lock()
                .map_err(|_| anyhow::anyhow!("chat cache lock was poisoned"))?;
            conversation_details_json(&cfg, connection, &info, &mut caches.detail, &query)
        })
        .await?;
    json_body(body)
}

async fn api_messages(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let cfg = state.cfg.clone();
    let info = state.info.clone();
    let friend_social = state.friend_social.clone();
    let body = state
        .chat
        .run(move |connection| {
            let body = messages_json(&cfg, connection, &info, &friend_social, &query)?;
            enrich_chat_images(&cfg.root, &cfg.account, body)
        })
        .await?;
    json_body(body)
}

async fn api_message_detail(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let cfg = state.cfg.clone();
    let info = state.info.clone();
    let friend_social = state.friend_social.clone();
    let body = state
        .chat
        .run(move |connection| {
            let body = message_detail_json(&cfg, connection, &info, &friend_social, &query)?;
            enrich_chat_images(&cfg.root, &cfg.account, body)
        })
        .await?;
    json_body(body)
}

#[cfg(feature = "image-index")]
fn enrich_chat_images(root: &Path, account: &str, body: String) -> anyhow::Result<String> {
    let mut value: Value = serde_json::from_str(&body)?;
    let mut paths = HashSet::new();
    collect_object_paths(&value, &mut paths);
    let ids = image_index_web::ids_for_paths(root, account, &paths).unwrap_or_default();
    annotate_object_paths(&mut value, &ids);
    Ok(serde_json::to_string(&value)?)
}

#[cfg(not(feature = "image-index"))]
fn enrich_chat_images(_root: &Path, _account: &str, body: String) -> anyhow::Result<String> {
    Ok(body)
}

#[cfg(feature = "image-index")]
fn collect_object_paths(value: &Value, paths: &mut HashSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_object_paths(value, paths);
            }
        }
        Value::Object(object) => {
            if let Some(path) = object.get("path").and_then(Value::as_str) {
                paths.insert(path.to_string());
            }
            for value in object.values() {
                collect_object_paths(value, paths);
            }
        }
        _ => {}
    }
}

#[cfg(feature = "image-index")]
fn annotate_object_paths(value: &mut Value, ids: &HashMap<String, i64>) {
    match value {
        Value::Array(values) => {
            for value in values {
                annotate_object_paths(value, ids);
            }
        }
        Value::Object(object) => {
            if let Some(id) = object
                .get("path")
                .and_then(Value::as_str)
                .and_then(|path| ids.get(path))
                .copied()
            {
                object.insert("image_index_id".to_string(), json!(id));
                object.insert(
                    "similar_href".to_string(),
                    json!(format!("/images?similar={id}")),
                );
            }
            for value in object.values_mut() {
                annotate_object_paths(value, ids);
            }
        }
        _ => {}
    }
}

fn status_json(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
) -> anyhow::Result<String> {
    let tables: i64 = con.query_row(
        "select count(*) from sqlite_master where type='table' and (name glob 'buddy_*' or name glob 'group_*' or name glob 'discuss_*' or name glob 'system_*')",
        [],
        |row| row.get(0),
    )?;
    let catalog_summary = if cfg.catalog.is_file() {
        catalog::summarize(&cfg.catalog).ok()
    } else {
        None
    };
    Ok(json!({
        "server": "rust-axum",
        "tables": tables,
        "catalog": {"path": cfg.catalog, "available": cfg.catalog.is_file(), "summary": catalog_summary},
        "data_sources": {
            "msg3_db": {"path": cfg.db, "source": cfg.db_source, "exists": cfg.db.is_file()},
            "infostorage": {"path": cfg.info_root, "source": cfg.info_root_source, "exists": cfg.info_root.is_dir()}
        },
        "infostorage": {"available": info.available(), "mode": "on_demand_no_disk_cache"},
        "asset_scan": {"running": false, "complete": true, "mode": "on_demand", "indexed_assets": 0, "active_assets": 0, "error": null},
        "image_index": image_index_status_value(&cfg.root, &cfg.account)
    })
    .to_string())
}

#[cfg(feature = "image-index")]
fn image_index_status_value(root: &Path, account: &str) -> Value {
    // `/api/status` is on the chat startup path. Recounting every embedding in
    // the 56 GiB manifest here used to make this otherwise tiny endpoint take
    // tens of seconds. The overview cache is refreshed transactionally by
    // index/link maintenance and is the correct low-latency status source.
    match image_index_web::overview(root, account) {
        Ok(mut status) => {
            status["available"] = json!(true);
            status
        }
        Err(error) => json!({"available": false, "error": error.to_string()}),
    }
}

#[cfg(not(feature = "image-index"))]
fn image_index_status_value(_root: &Path, _account: &str) -> Value {
    json!({"available": false, "error": "image-index feature is not enabled in this build"})
}

#[cfg(feature = "image-index")]
async fn legacy_image_status(State(state): State<AppState>) -> Result<Response, ApiError> {
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    let value = tokio::task::spawn_blocking(move || image_index::status(&root, &account))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))??;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(not(feature = "image-index"))]
async fn legacy_image_status(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(Json(image_index_status_value(
        &state.cfg.root,
        &state.cfg.account,
    ))
    .into_response())
}

#[cfg(feature = "image-index")]
#[derive(Clone)]
struct MaintenanceController {
    runtime: Arc<Mutex<MaintenanceRuntime>>,
}

#[cfg(feature = "image-index")]
#[derive(Default)]
struct MaintenanceRuntime {
    running: bool,
    stopping: bool,
    cancel: Option<Arc<AtomicBool>>,
    started_at: Option<String>,
    last_error: Option<String>,
}

#[cfg(feature = "image-index")]
impl Default for MaintenanceController {
    fn default() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(MaintenanceRuntime::default())),
        }
    }
}

#[cfg(feature = "image-index")]
impl MaintenanceController {
    fn snapshot(&self) -> Value {
        let runtime = self.runtime.lock().expect("maintenance lock poisoned");
        json!({
            "supported": true,
            "running": runtime.running,
            "stopping": runtime.stopping,
            "started_at": runtime.started_at,
            "last_error": runtime.last_error,
        })
    }

    fn start(&self, cfg: Arc<Config>, task_id: &str) -> Result<Value, ApiError> {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut runtime = self.runtime.lock().expect("maintenance lock poisoned");
            if runtime.running {
                return Ok(self.snapshot_unlocked(&runtime));
            }
            runtime.running = true;
            runtime.stopping = false;
            runtime.cancel = Some(cancel.clone());
            runtime.started_at = Some(chrono::Utc::now().to_rfc3339());
            runtime.last_error = None;
        }
        let shared = self.runtime.clone();
        let task_id = task_id.to_string();
        std::thread::Builder::new()
            .name(format!("qq-{task_id}-maintenance"))
            .spawn(move || {
                let result = if task_id == "chat-provenance" {
                    image_index_link::link_chat(image_index_link::LinkChatOptions {
                        root: cfg.root.clone(),
                        account: cfg.account.clone(),
                        manifest_path: None,
                        chat_db: cfg.db.clone(),
                        workers: image_index_link::recommended_link_workers(),
                        batch_size: image_index_link::DEFAULT_LINK_BATCH_SIZE,
                        force: false,
                        max_rows: None,
                        cancel: Some(cancel.clone()),
                    })
                    .map(|_| ())
                } else {
                    image_insights::build_popularity_analysis(
                        &cfg.root,
                        &cfg.account,
                        &cfg.db,
                        Some(&cancel),
                    )
                    .map(|_| ())
                };
                let cleanup_superseded_occurrences =
                    task_id == "image-popularity-analysis" && result.is_ok();
                let mut runtime = shared.lock().expect("maintenance lock poisoned");
                runtime.running = false;
                runtime.stopping = false;
                runtime.cancel = None;
                runtime.last_error = if cancel.load(Ordering::Relaxed) {
                    None
                } else {
                    result.err().map(|error| error.to_string())
                };
                drop(runtime);
                if cleanup_superseded_occurrences {
                    if let Err(error) = image_insights::cleanup_superseded_occurrence_tables(
                        &cfg.root,
                        &cfg.account,
                    ) {
                        tracing::warn!(
                            %error,
                            "superseded popularity occurrence table cleanup failed"
                        );
                    }
                }
            })
            .map_err(|error| {
                let mut runtime = self.runtime.lock().expect("maintenance lock poisoned");
                runtime.running = false;
                runtime.cancel = None;
                ApiError::internal(format!("failed to start maintenance task: {error}"))
            })?;
        Ok(self.snapshot())
    }

    fn pause(&self) -> Value {
        let mut runtime = self.runtime.lock().expect("maintenance lock poisoned");
        if runtime.running {
            runtime.stopping = true;
            if let Some(cancel) = &runtime.cancel {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        self.snapshot_unlocked(&runtime)
    }

    fn snapshot_unlocked(&self, runtime: &MaintenanceRuntime) -> Value {
        json!({
            "supported": true,
            "running": runtime.running,
            "stopping": runtime.stopping,
            "started_at": runtime.started_at,
            "last_error": runtime.last_error,
        })
    }
}

#[cfg(feature = "image-index")]
#[derive(Clone)]
struct SearchActor {
    sender: mpsc::Sender<SearchCommand>,
    metrics: Arc<SearchMetrics>,
}

#[cfg(feature = "image-index")]
#[derive(Default)]
struct SearchMetrics {
    queued: AtomicUsize,
    active: AtomicBool,
    completed: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
}

#[cfg(feature = "image-index")]
struct TemporaryUpload {
    path: PathBuf,
}

#[cfg(feature = "image-index")]
impl TemporaryUpload {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(feature = "image-index")]
impl Drop for TemporaryUpload {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "failed to remove query upload"
                );
            }
        }
    }
}

#[cfg(feature = "image-index")]
enum SearchKind {
    Text {
        text: String,
        strategy: image_index::QueryStrategy,
        limit: usize,
    },
    Asset {
        encoded_asset: String,
        mode: image_index::QueryMode,
        strategy: image_index::QueryStrategy,
        limit: usize,
    },
    Upload {
        upload: TemporaryUpload,
        mode: image_index::QueryMode,
        strategy: image_index::QueryStrategy,
        limit: usize,
    },
}

#[cfg(feature = "image-index")]
struct SearchCommand {
    kind: SearchKind,
    response: oneshot::Sender<Result<Value, String>>,
}

#[cfg(feature = "image-index")]
impl SearchActor {
    fn start(root: PathBuf, account: String) -> Self {
        let mut cache = image_index::QueryRuntimeCache::default();
        Self::start_with_worker(move |kind| {
            run_search_command(&root, &account, kind, &mut cache).map_err(|error| error.to_string())
        })
    }

    fn start_with_worker<F>(mut worker: F) -> Self
    where
        F: FnMut(SearchKind) -> Result<Value, String> + Send + 'static,
    {
        let (sender, mut receiver) = mpsc::channel::<SearchCommand>(SEARCH_QUEUE_CAPACITY);
        let metrics = Arc::new(SearchMetrics::default());
        let worker_metrics = metrics.clone();
        std::thread::Builder::new()
            .name("qq-image-search".to_string())
            .spawn(move || {
                while let Some(command) = receiver.blocking_recv() {
                    let _ = worker_metrics.queued.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |queued| Some(queued.saturating_sub(1)),
                    );
                    // Consuming AbortSignal in the browser closes this
                    // response channel when an image/search is replaced. Do
                    // not spend GPU time on a result nobody can receive.
                    if command.response.is_closed() {
                        worker_metrics.cancelled.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    worker_metrics.active.store(true, Ordering::Relaxed);
                    let result = worker(command.kind);
                    worker_metrics.active.store(false, Ordering::Relaxed);
                    if result.is_ok() {
                        worker_metrics.completed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        worker_metrics.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = command.response.send(result);
                }
            })
            .expect("failed to start image search actor");
        Self { sender, metrics }
    }

    async fn execute(&self, kind: SearchKind) -> Result<Value, ApiError> {
        let (sender, receiver) = oneshot::channel();
        self.metrics.queued.fetch_add(1, Ordering::Relaxed);
        if let Err(error) = self.sender.try_send(SearchCommand {
            kind,
            response: sender,
        }) {
            self.metrics.queued.fetch_sub(1, Ordering::Relaxed);
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "search_queue_full",
                    "image search queue is full; retry later",
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    ApiError::unavailable("image search actor is unavailable")
                }
            });
        }
        match tokio::time::timeout(SEARCH_TIMEOUT, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(ApiError::internal(message)),
            Ok(Err(_)) => Err(ApiError::unavailable("image search actor stopped")),
            Err(_) => Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "search_timeout",
                "image search exceeded the five-minute timeout",
            )),
        }
    }

    fn metrics_json(&self) -> Value {
        json!({
            "active": self.metrics.active.load(Ordering::Relaxed),
            "queued": self.metrics.queued.load(Ordering::Relaxed),
            "capacity": SEARCH_QUEUE_CAPACITY,
            "completed": self.metrics.completed.load(Ordering::Relaxed),
            "failed": self.metrics.failed.load(Ordering::Relaxed),
            "cancelled": self.metrics.cancelled.load(Ordering::Relaxed),
        })
    }
}

#[cfg(feature = "image-index")]
fn run_search_command(
    root: &Path,
    account: &str,
    kind: SearchKind,
    cache: &mut image_index::QueryRuntimeCache,
) -> anyhow::Result<Value> {
    let body = match kind {
        SearchKind::Text {
            text,
            strategy,
            limit,
        } => image_index::search_text_json_cached_with_strategy(
            root, account, &text, strategy, limit, cache,
        )?,
        SearchKind::Asset {
            encoded_asset,
            mode,
            strategy,
            limit,
        } => image_index::similar_asset_json_cached_with_strategy(
            root,
            account,
            &encoded_asset,
            mode,
            strategy,
            limit,
            cache,
        )?,
        SearchKind::Upload {
            upload,
            mode,
            strategy,
            limit,
        } => image_index::query_uploaded_image_json_cached_with_strategy(
            root,
            account,
            upload.path(),
            mode,
            strategy,
            limit,
            cache,
        )?,
    };
    Ok(serde_json::from_str(&body)?)
}

#[cfg(feature = "image-index")]
fn search_options(
    query: &HashMap<String, String>,
) -> Result<(image_index::QueryMode, image_index::QueryStrategy, usize), ApiError> {
    let mode =
        image_index::QueryMode::parse(query.get("mode").map(String::as_str).unwrap_or("all"))
            .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let strategy = image_index::QueryStrategy::parse(
        query
            .get("strategy")
            .or_else(|| query.get("query_strategy"))
            .map(String::as_str)
            .unwrap_or("exact"),
    )
    .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let limit = query
        .get("limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(50usize)
        .clamp(1, 100);
    Ok((mode, strategy, limit))
}

#[cfg(feature = "image-index")]
async fn legacy_text_search(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let (_, strategy, limit) = search_options(&query)?;
    let text = query.get("q").cloned().unwrap_or_default();
    let value = state
        .search
        .execute(SearchKind::Text {
            text,
            strategy,
            limit,
        })
        .await?;
    Ok(Json(value).into_response())
}

#[cfg(feature = "image-index")]
async fn legacy_similar_search(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let asset = query
        .get("asset")
        .cloned()
        .ok_or_else(|| ApiError::bad_request("asset query parameter is required"))?;
    let (mode, strategy, limit) = search_options(&query)?;
    let value = state
        .search
        .execute(SearchKind::Asset {
            encoded_asset: asset,
            mode,
            strategy,
            limit,
        })
        .await?;
    Ok(Json(value).into_response())
}

#[cfg(not(feature = "image-index"))]
async fn legacy_text_search(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    Ok(Json(json!({
        "account": state.cfg.account,
        "text": query.get("q").cloned().unwrap_or_default(),
        "results": [],
        "unavailable": [{
            "signal": "semantic_descriptor:clip_vec",
            "reason": "image-index feature is not enabled in this build"
        }]
    }))
    .into_response())
}

#[cfg(not(feature = "image-index"))]
async fn legacy_similar_search(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(Json(json!({
        "account": state.cfg.account,
        "results": [],
        "unavailable": [{
            "signal": "image_index",
            "reason": "image-index feature is not enabled in this build"
        }]
    }))
    .into_response())
}

async fn legacy_asset(
    State(state): State<AppState>,
    RoutePath(encoded): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let path = decode_asset_path(&state.cfg.root, &encoded)
        .map_err(|error| ApiError::not_found(error.to_string()))?;
    stream_file(&path, &headers, "private, max-age=3600").await
}

#[cfg(feature = "image-index")]
async fn image_overview(State(state): State<AppState>) -> Result<Response, ApiError> {
    let value = cached_image_overview(&state).await?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(feature = "image-index")]
async fn image_insights_overview(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let query = image_insights::InsightsQuery::from_params(&params).map_err(ApiError::from)?;
    let cfg = state.cfg.clone();
    let cache = state.insights_overview_cache.clone();
    let info = state.info.clone();
    let caches = state.caches.clone();
    let value = state
        .chat
        .run(move |connection| {
            let generation = image_insights::overview_cache_identity(&cfg.root, &cfg.account)?;
            let key = format!("{}:{}:{}", cfg.account, generation, query.overview_key());
            if let Some(value) = cache
                .lock()
                .map_err(|_| anyhow::anyhow!("insights overview cache poisoned"))?
                .get(&key)
            {
                return Ok(value);
            }
            let mut value = image_insights::overview(&cfg.root, &cfg.account, &query)?;
            {
                let mut caches = caches
                    .lock()
                    .map_err(|_| anyhow::anyhow!("chat cache lock was poisoned"))?;
                enrich_insights_overview_labels(
                    &cfg,
                    connection,
                    &info,
                    &mut caches.detail,
                    &mut value,
                );
            }
            let mut cache = cache
                .lock()
                .map_err(|_| anyhow::anyhow!("insights overview cache poisoned"))?;
            cache.insert(key, value.clone());
            Ok(value)
        })
        .await?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(feature = "image-index")]
async fn image_insights_assets(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let query = image_insights::InsightsQuery::from_params(&params).map_err(ApiError::from)?;
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(30)
        .clamp(1, 100);
    let cursor = params.get("cursor").map(String::as_str);
    let request_token = params.get("request_token").map(String::as_str);
    let value = state
        .ranking_tasks
        .request(
            state.cfg.root.clone(),
            state.cfg.account.clone(),
            query,
            cursor,
            limit,
            request_token,
        )
        .await
        .map_err(ApiError::from)?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(feature = "image-index")]
async fn image_insights_compare(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let query = image_insights::InsightsQuery::from_params(&params).map_err(ApiError::from)?;
    let by = params
        .get("by")
        .map(String::as_str)
        .unwrap_or("sender")
        .to_string();
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    let same_cache = state.same_image_cache.clone();
    let cache = state.insights_compare_cache.clone();
    blocking_value(move || {
        let generation = same_cache.result_cache_identity(&root, &account)?;
        let key = format!("{account}:{generation}:{by}:{}", query.task_key());
        if let Some(value) = cache
            .lock()
            .map_err(|_| anyhow::anyhow!("insights compare cache poisoned"))?
            .get(&key)
        {
            return Ok(value);
        }
        let value = image_insights::compare(&root, &account, &query, &by, &same_cache)?;
        cache
            .lock()
            .map_err(|_| anyhow::anyhow!("insights compare cache poisoned"))?
            .insert(key, value.clone());
        Ok(value)
    })
    .await
}

#[cfg(feature = "image-index")]
async fn maintenance_tasks(State(state): State<AppState>) -> Result<Response, ApiError> {
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    let mut value =
        tokio::task::spawn_blocking(move || image_index_web::maintenance_tasks(&root, &account))
            .await
            .map_err(|error| ApiError::internal(error.to_string()))??;
    value["search_queue"] = state.search.metrics_json();
    value["same_image_cache"] = state.same_image_cache.metrics_json();
    value["ranking_tasks"] = state.ranking_tasks.metrics_json();
    if let Some(task) = value["tasks"].as_array_mut().and_then(|tasks| {
        tasks
            .iter_mut()
            .find(|task| task["id"] == "chat-provenance")
    }) {
        let control = state.maintenance.snapshot();
        if !control["running"].as_bool().unwrap_or(false) && task["status"] == "running" {
            task["status"] = json!("paused");
        }
        task["control"] = control;
    }
    if let Some(task) = value["tasks"].as_array_mut().and_then(|tasks| {
        tasks
            .iter_mut()
            .find(|task| task["id"] == "image-popularity-analysis")
    }) {
        let control = state.popularity_maintenance.snapshot();
        if !control["running"].as_bool().unwrap_or(false) && task["status"] == "running" {
            task["status"] = json!("paused");
        }
        task["control"] = control;
    }
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(feature = "image-index")]
async fn maintenance_task_start(
    State(state): State<AppState>,
    RoutePath(task_id): RoutePath<String>,
) -> Result<Response, ApiError> {
    ensure_controllable_task(&task_id)?;
    let controller = if task_id == "chat-provenance" {
        &state.maintenance
    } else {
        &state.popularity_maintenance
    };
    let control = controller.start(state.cfg.clone(), &task_id)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"task_id": task_id, "control": control})),
    )
        .into_response())
}

#[cfg(feature = "image-index")]
async fn maintenance_task_pause(
    State(state): State<AppState>,
    RoutePath(task_id): RoutePath<String>,
) -> Result<Response, ApiError> {
    ensure_controllable_task(&task_id)?;
    let controller = if task_id == "chat-provenance" {
        &state.maintenance
    } else {
        &state.popularity_maintenance
    };
    let control = controller.pause();
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"task_id": task_id, "control": control})),
    )
        .into_response())
}

#[cfg(feature = "image-index")]
fn ensure_controllable_task(task_id: &str) -> Result<(), ApiError> {
    if matches!(task_id, "chat-provenance" | "image-popularity-analysis") {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "maintenance task {task_id} cannot be controlled by the web service"
        )))
    }
}

#[cfg(feature = "image-index")]
async fn cached_image_overview(state: &AppState) -> Result<Value, ApiError> {
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    tokio::task::spawn_blocking(move || image_index_web::overview(&root, &account))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::from)
}

#[cfg(feature = "image-index")]
async fn image_assets(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    let same_cache = state.same_image_cache.clone();
    blocking_value(move || {
        let mut value = image_index_web::assets(&root, &account, &query)?;
        let folds_variants = query
            .get("duplicates")
            .or_else(|| query.get("duplicate"))
            .map(String::as_str)
            .unwrap_or("collapsed")
            == "variants";
        if folds_variants {
            let mut seen = HashSet::new();
            if let Some(items) = value["items"].as_array_mut() {
                items.retain(|item| {
                    let Some(id) = item["id"].as_i64() else {
                        return true;
                    };
                    same_cache
                        .peek(id)
                        .map(|group| seen.insert(group.canonical_key()))
                        .unwrap_or(true)
                });
            }
        }
        Ok(value)
    })
    .await
}

#[cfg(feature = "image-index")]
async fn image_asset_detail(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
) -> Result<Response, ApiError> {
    let id = parse_asset_id(&id)?;
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    let value =
        tokio::task::spawn_blocking(move || image_index_web::asset_detail(&root, &account, id))
            .await
            .map_err(|error| ApiError::internal(error.to_string()))??
            .ok_or_else(|| ApiError::not_found(format!("image asset {id} was not found")))?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(feature = "image-index")]
async fn image_asset_occurrences(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let id = parse_asset_id(&id)?;
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    blocking_value(move || image_index_web::occurrences(&root, &account, id, &query)).await
}

#[cfg(feature = "image-index")]
async fn image_asset_reference_analysis(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let id = parse_asset_id(&id)?;
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    let same_image_cache = state.same_image_cache.clone();
    let cached_group = same_image_cache.peek(id);
    let same_image_warming = cached_group.is_none();
    let same_image_ids = cached_group
        .map(|group| group.asset_ids.clone())
        .unwrap_or_else(|| vec![id]);
    let occurrence_root = root.clone();
    let occurrence_account = account.clone();
    let (same_image_ids, references, scan) = tokio::task::spawn_blocking(move || {
        let (references, scan) = image_index_web::occurrence_keys_for_assets(
            &occurrence_root,
            &occurrence_account,
            &same_image_ids,
        )?;
        anyhow::Ok((same_image_ids, references, scan))
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))??;
    if same_image_warming {
        tokio::task::spawn_blocking(move || {
            let _ = same_image_cache.resolve(&root, &account, id);
        });
    }
    let references = references
        .into_iter()
        .map(|item| (item.table, item.rowid, item.linked_at))
        .collect();
    let cfg = state.cfg.clone();
    let info = state.info.clone();
    let friend_social = state.friend_social.clone();
    let caches = state.caches.clone();
    let body = state
        .chat
        .run(move |connection| {
            let mut caches = caches
                .lock()
                .map_err(|_| anyhow::anyhow!("chat cache lock was poisoned"))?;
            let body = image_reference_analysis_json(
                &cfg,
                connection,
                &info,
                &friend_social,
                &mut caches.detail,
                id,
                same_image_ids,
                references,
                scan,
                &query,
            )?;
            enrich_chat_images(&cfg.root, &cfg.account, body)
        })
        .await?;
    let mut value: Value =
        serde_json::from_str(&body).map_err(|error| ApiError::internal(error.to_string()))?;
    value["same_image_status"] = json!(if same_image_warming {
        "warming"
    } else {
        "complete"
    });
    json_body(value.to_string())
}

#[cfg(feature = "image-index")]
async fn image_asset_similar(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let id = parse_asset_id(&id)?;
    let (mode, strategy, limit) = search_options(&query)?;
    if mode != image_index::QueryMode::Exact {
        let root = state.cfg.root.clone();
        let account = state.cfg.account.clone();
        if let Some(value) = tokio::task::spawn_blocking(move || {
            image_index_web::pending_similarity_report(&root, &account, id)
        })
        .await
        .map_err(|error| ApiError::internal(error.to_string()))??
        {
            return Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response());
        }
    }
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    let mut value = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let body = image_index::similar_indexed_asset_json_with_strategy(
            &root, &account, id, mode, strategy, limit,
        )?;
        Ok(serde_json::from_str(&body)?)
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))??;
    image_index_web::sanitize_search_report(&state.cfg.root, &state.cfg.account, &mut value)?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(feature = "image-index")]
async fn image_asset_content(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = parse_asset_id(&id)?;
    let path = indexed_asset_path(&state, id).await?;
    stream_file(&path, &headers, "private, max-age=3600").await
}

#[cfg(feature = "image-index")]
async fn image_asset_thumbnail(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = parse_asset_id(&id)?;
    let edge = query
        .get("edge")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(320);
    if !matches!(edge, 160 | 320 | 640) {
        return Err(ApiError::bad_request("edge must be 160, 320, or 640"));
    }
    let source = indexed_asset_path(&state, id).await?;
    let metadata = tokio::fs::metadata(&source).await?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let cache_root =
        msg3_richtext_parser_rs::config::account_output_root(&state.cfg.root, &state.cfg.account)
            .join("web-cache")
            .join("thumbnails");
    let cache_path = cache_root.join(format!("{id}-{edge}-{}-{modified}.jpg", metadata.len()));
    if !cache_path.is_file() {
        let permit = state
            .thumbnail_workers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApiError::unavailable("thumbnail workers are unavailable"))?;
        if !cache_path.is_file() {
            let source = source.clone();
            let cache_path_copy = cache_path.clone();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                generate_thumbnail(&source, &cache_path_copy, edge)
            })
            .await
            .map_err(|error| ApiError::internal(error.to_string()))??;
        }
    }
    stream_file(
        &cache_path,
        &headers,
        "private, max-age=31536000, immutable",
    )
    .await
}

#[cfg(feature = "image-index")]
async fn image_text_search(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let text = query
        .get("q")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("q query parameter is required"))?
        .to_string();
    let (_, strategy, limit) = search_options(&query)?;
    let overview = cached_image_overview(&state).await?;
    if overview["coverage"]["clip_assets"].as_i64().unwrap_or(0) == 0 {
        return Ok((
            [(CACHE_CONTROL, "no-store")],
            Json(json!({
                "account": state.cfg.account,
                "text": text,
                "results": [],
                "unavailable": [{
                "signal": "semantic_descriptor:clip_vec",
                "reason": "图像索引尚未包含 CLIP 语义向量"
                }]
            })),
        )
            .into_response());
    }
    let mut value = state
        .search
        .execute(SearchKind::Text {
            text,
            strategy,
            limit,
        })
        .await?;
    image_index_web::sanitize_search_report(&state.cfg.root, &state.cfg.account, &mut value)?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(feature = "image-index")]
async fn image_upload_search(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<Response, ApiError> {
    let mut multipart = multipart.map_err(|error| ApiError::bad_request(error.to_string()))?;
    let mut bytes = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?
    {
        if field.name() != Some("image") {
            continue;
        }
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|error| ApiError::bad_request(error.to_string()))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_UPLOAD_BYTES {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "upload_too_large",
                    "image upload exceeds 32 MiB",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        break;
    }
    if bytes.is_empty() {
        return Err(ApiError::bad_request(
            "multipart field named image is required",
        ));
    }
    image::guess_format(&bytes).map_err(|_| {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_image",
            "uploaded data has no recognized image signature",
        )
    })?;
    let validation = bytes.clone();
    tokio::task::spawn_blocking(move || image::load_from_memory(&validation).map(|_| ()))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(|error| {
            ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "invalid_image",
                error.to_string(),
            )
        })?;
    let overview = cached_image_overview(&state).await?;
    let assets = overview["health"]["assets"].as_i64().unwrap_or(0);
    let pending = overview["distributions"]["quality"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["key"] == "manifest_pending")
        .and_then(|item| item["count"].as_i64())
        .unwrap_or(0);
    if assets > 0 && pending >= assets {
        return Ok(([(CACHE_CONTROL, "no-store")], Json(json!({
            "account": state.cfg.account,
            "results": [],
            "unavailable": [
                {"signal": "pixel_fingerprint", "reason": "图像索引仍在等待像素与结构指纹"},
                {"signal": "copy_descriptor:sscd_vec", "reason": "图像索引尚未包含 SSCD 视觉副本向量"},
                {"signal": "semantic_descriptor:clip_vec", "reason": "图像索引尚未包含 CLIP 语义向量"}
            ]
        }))).into_response());
    }
    let upload_root =
        msg3_richtext_parser_rs::config::account_output_root(&state.cfg.root, &state.cfg.account)
            .join("web-cache")
            .join("uploads");
    tokio::fs::create_dir_all(&upload_root).await?;
    static UPLOAD_ID: AtomicU64 = AtomicU64::new(1);
    let upload_path = upload_root.join(format!(
        "query-{}-{}.img",
        std::process::id(),
        UPLOAD_ID.fetch_add(1, Ordering::Relaxed)
    ));
    tokio::fs::write(&upload_path, &bytes).await?;
    let upload = TemporaryUpload::new(upload_path);
    let (mode, strategy, limit) = search_options(&query)?;
    let mut value = state
        .search
        .execute(SearchKind::Upload {
            upload,
            mode,
            strategy,
            limit,
        })
        .await?;
    image_index_web::sanitize_search_report(&state.cfg.root, &state.cfg.account, &mut value)?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(feature = "image-index")]
async fn indexed_asset_path(state: &AppState, id: i64) -> Result<PathBuf, ApiError> {
    if let Some(path) = state
        .asset_path_cache
        .lock()
        .expect("asset path cache lock poisoned")
        .get(&id)
        .cloned()
    {
        return Ok(path);
    }
    let root = state.cfg.root.clone();
    let account = state.cfg.account.clone();
    let path =
        tokio::task::spawn_blocking(move || image_index_web::asset_path(&root, &account, id))
            .await
            .map_err(|error| ApiError::internal(error.to_string()))??
            .ok_or_else(|| ApiError::not_found(format!("image asset {id} was not found")))?;
    let path = secure_path(&state.cfg.root, &path)?;
    let mut cache = state
        .asset_path_cache
        .lock()
        .expect("asset path cache lock poisoned");
    if cache.len() >= ASSET_PATH_CACHE_CAPACITY {
        cache.clear();
    }
    cache.insert(id, path.clone());
    Ok(path)
}

#[cfg(feature = "image-index")]
fn parse_asset_id(value: &str) -> Result<i64, ApiError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| ApiError::bad_request("asset id must be a positive integer"))
}

#[cfg(feature = "image-index")]
async fn blocking_value<F>(operation: F) -> Result<Response, ApiError>
where
    F: FnOnce() -> anyhow::Result<Value> + Send + 'static,
{
    let value = tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))??;
    Ok(([(CACHE_CONTROL, "no-store")], Json(value)).into_response())
}

#[cfg(not(feature = "image-index"))]
async fn image_overview() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_insights_overview() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_insights_assets() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_insights_compare() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn maintenance_tasks() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn maintenance_task_start() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn maintenance_task_pause() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_assets() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_asset_detail() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_asset_occurrences() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_asset_reference_analysis() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_asset_similar() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_asset_content() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_asset_thumbnail() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_text_search() -> Result<Response, ApiError> {
    image_feature_disabled()
}
#[cfg(not(feature = "image-index"))]
async fn image_upload_search() -> Result<Response, ApiError> {
    image_feature_disabled()
}

#[cfg(not(feature = "image-index"))]
fn image_feature_disabled() -> Result<Response, ApiError> {
    Err(ApiError::unavailable(
        "image-index feature is not enabled in this build",
    ))
}

#[cfg(feature = "image-index")]
fn generate_thumbnail(source: &Path, destination: &Path, edge: u32) -> anyhow::Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // QQ media extensions are not trustworthy (GIF/WebP payloads are often
    // stored under `.jpg`). Detect from the file signature before decoding.
    let image = image::ImageReader::open(source)?
        .with_guessed_format()?
        .decode()?;
    let thumbnail = image.thumbnail(edge, edge).to_rgb8();
    static THUMBNAIL_ID: AtomicU64 = AtomicU64::new(1);
    let temporary = destination.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        THUMBNAIL_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = thumbnail.save_with_format(&temporary, image::ImageFormat::Jpeg);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    match std::fs::rename(&temporary, destination) {
        Ok(()) => Ok(()),
        Err(_) if destination.is_file() => {
            let _ = std::fs::remove_file(temporary);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            Err(error.into())
        }
    }
}

#[cfg(feature = "image-index")]
fn secure_path(root: &Path, path: &Path) -> Result<PathBuf, ApiError> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| ApiError::not_found(error.to_string()))?;
    let canonical_path = path
        .canonicalize()
        .map_err(|error| ApiError::not_found(error.to_string()))?;
    if !canonical_path.starts_with(canonical_root) {
        return Err(ApiError::not_found("asset is outside the workspace root"));
    }
    Ok(canonical_path)
}

async fn stream_file(
    path: &Path,
    request_headers: &HeaderMap,
    cache_control: &'static str,
) -> Result<Response, ApiError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ApiError::not_found(error.to_string()))?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(ApiError::not_found("asset is not a regular file"));
    }
    let length = metadata.len();
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let modified_seconds = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let etag = format!("W/\"{length:x}-{modified_seconds:x}\"");
    if request_headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|value| value.trim() == etag))
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(ETAG, etag)
            .header(CACHE_CONTROL, cache_control)
            .body(Body::empty())
            .expect("valid not-modified response"));
    }
    let range = match request_headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => parse_byte_range(value, length),
        None => Ok(None),
    };
    let (start, end, status) = match range {
        Ok(Some((start, end))) => (start, end, StatusCode::PARTIAL_CONTENT),
        Ok(None) => (0, length.saturating_sub(1), StatusCode::OK),
        Err(()) => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(CONTENT_RANGE, format!("bytes */{length}"))
                .header(ETAG, etag)
                .body(Body::empty())
                .expect("valid range response"));
        }
    };
    let body_length = if length == 0 { 0 } else { end - start + 1 };
    let mut sniff = [0u8; 16];
    file.seek(SeekFrom::Start(0)).await?;
    let sniff_len = file.read(&mut sniff).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let content_type = content_type_for_path(path, &sniff[..sniff_len]);
    let stream = ReaderStream::new(file.take(body_length));
    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, body_length)
        .header(ACCEPT_RANGES, "bytes")
        .header(ETAG, etag)
        .header(CACHE_CONTROL, cache_control)
        .header(LAST_MODIFIED, httpdate::fmt_http_date(modified));
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(CONTENT_RANGE, format!("bytes {start}-{end}/{length}"));
    }
    Ok(builder
        .body(Body::from_stream(stream))
        .expect("valid streaming response"))
}

fn parse_byte_range(value: &str, length: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(value) = value.strip_prefix("bytes=") else {
        return Err(());
    };
    if value.contains(',') || length == 0 {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let start = length.saturating_sub(suffix);
        return Ok(Some((start, length - 1)));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= length {
        return Err(());
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(length - 1)
    };
    if end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

async fn spa_fallback(uri: Uri) -> Result<Response, ApiError> {
    let path = uri.path().trim_start_matches('/');
    if path == "api" || path.starts_with("api/") || path == "asset" || path.starts_with("asset/") {
        return Err(ApiError::not_found(format!(
            "route {} was not found",
            uri.path()
        )));
    }
    #[cfg(feature = "web-ui")]
    {
        if !path.is_empty() {
            if let Some(asset) = WebAssets::get(path) {
                return Ok(embedded_response(path, asset.data.as_ref()));
            }
            if path.starts_with("assets/") {
                return Err(ApiError::not_found("embedded web asset was not found"));
            }
        }
        let index = WebAssets::get("index.html")
            .ok_or_else(|| ApiError::unavailable("embedded web UI is missing index.html"))?;
        return Ok(embedded_response("index.html", index.data.as_ref()));
    }
    #[cfg(not(feature = "web-ui"))]
    {
        let _ = path;
        Err(ApiError::unavailable(
            "web UI is not embedded in this build; rebuild with --features web-ui",
        ))
    }
}

#[cfg(feature = "web-ui")]
fn embedded_response(path: &str, data: &[u8]) -> Response {
    let mime = match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
    {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "json" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    };
    let cache = if path == "index.html" {
        "no-cache"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    let etag = format!("\"{:x}\"", md5::compute(data));
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, mime)
        .header(CONTENT_LENGTH, data.len())
        .header(CACHE_CONTROL, cache)
        .header(ETAG, etag)
        .body(Body::from(data.to_vec()))
        .expect("valid embedded response")
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_validation_rejects_remote_bindings() {
        assert!(validate_loopback_host("127.0.0.1").is_ok());
        assert!(validate_loopback_host("::1").is_ok());
        assert!(validate_loopback_host("localhost").is_ok());
        assert!(validate_loopback_host("0.0.0.0").is_err());
        assert!(validate_loopback_host("192.168.1.2").is_err());
    }

    #[test]
    fn byte_ranges_cover_normal_open_and_suffix_forms() {
        assert_eq!(parse_byte_range("bytes=0-9", 100), Ok(Some((0, 9))));
        assert_eq!(parse_byte_range("bytes=90-", 100), Ok(Some((90, 99))));
        assert_eq!(parse_byte_range("bytes=-10", 100), Ok(Some((90, 99))));
        assert!(parse_byte_range("bytes=100-101", 100).is_err());
        assert!(parse_byte_range("bytes=0-1,5-6", 100).is_err());
    }

    #[tokio::test]
    async fn range_response_sniffs_mime_from_file_start() {
        let path = std::env::temp_dir().join(format!(
            "qq-analyzer-range-mime-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"\x89PNG\r\n\x1a\npayload-after-signature").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, "bytes=12-".parse().unwrap());
        let response = stream_file(&path, &headers, "no-store").await.unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "image/png");
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "image-index")]
    #[test]
    fn thumbnail_generation_uses_magic_instead_of_qq_file_extension() {
        let root = std::env::temp_dir().join(format!(
            "qq-analyzer-thumbnail-magic-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("actually-a-gif.jpg");
        let thumbnail = root.join("thumbnail.jpg");
        image::DynamicImage::new_rgb8(4, 3)
            .save_with_format(&source, image::ImageFormat::Gif)
            .unwrap();

        generate_thumbnail(&source, &thumbnail, 2).unwrap();

        assert_eq!(
            image::guess_format(&std::fs::read(&thumbnail).unwrap()).unwrap(),
            image::ImageFormat::Jpeg
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "image-index")]
    #[tokio::test]
    async fn blocked_search_actor_saturates_at_eight_without_blocking_chat_pool() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut first = true;
        let actor = SearchActor::start_with_worker(move |_| {
            if first {
                first = false;
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            }
            Ok(json!({"results": [], "unavailable": []}))
        });
        let request = || SearchKind::Text {
            text: "fixture".to_string(),
            strategy: image_index::QueryStrategy::Exact,
            limit: 1,
        };
        let (first_tx, _first_rx) = oneshot::channel();
        actor
            .sender
            .try_send(SearchCommand {
                kind: request(),
                response: first_tx,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let mut queued_receivers = Vec::new();
        for _ in 0..SEARCH_QUEUE_CAPACITY {
            let (response, receiver) = oneshot::channel();
            actor
                .sender
                .try_send(SearchCommand {
                    kind: request(),
                    response,
                })
                .unwrap();
            queued_receivers.push(receiver);
        }
        let (overflow, _receiver) = oneshot::channel();
        assert!(matches!(
            actor.sender.try_send(SearchCommand {
                kind: request(),
                response: overflow,
            }),
            Err(mpsc::error::TrySendError::Full(_))
        ));

        let db = std::env::temp_dir().join(format!(
            "qq-analyzer-chat-pool-{}.sqlite",
            std::process::id()
        ));
        drop(Connection::open(&db).unwrap());
        let pool = ChatDbPool::open(&db, CHAT_POOL_SIZE).unwrap();
        let value = tokio::time::timeout(
            Duration::from_secs(1),
            pool.run(|con| Ok(con.query_row("select 1", [], |row| row.get::<_, i64>(0))?)),
        )
        .await
        .expect("chat query was blocked by image inference")
        .unwrap();
        assert_eq!(value, 1);
        let _ = release_tx.send(());
        drop(queued_receivers);
        let _ = std::fs::remove_file(db);
    }

    #[cfg(feature = "image-index")]
    #[test]
    fn cancelled_queued_upload_is_removed_by_search_actor_ownership() {
        let path = std::env::temp_dir().join(format!(
            "qq-analyzer-cancelled-upload-{}-{}.img",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"fixture").unwrap();
        let actor = SearchActor::start_with_worker(|_| {
            panic!("cancelled upload must not reach the search worker")
        });
        let (response, receiver) = oneshot::channel();
        drop(receiver);
        actor
            .sender
            .try_send(SearchCommand {
                kind: SearchKind::Upload {
                    upload: TemporaryUpload::new(path.clone()),
                    mode: image_index::QueryMode::All,
                    strategy: image_index::QueryStrategy::Fast,
                    limit: 1,
                },
                response,
            })
            .unwrap();
        for _ in 0..200 {
            if !path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!path.exists());
        assert_eq!(actor.metrics.cancelled.load(Ordering::Relaxed), 1);
    }

    #[cfg(feature = "image-index")]
    #[test]
    fn search_actor_skips_requests_whose_http_receiver_was_cancelled() {
        let executions = Arc::new(AtomicUsize::new(0));
        let worker_executions = executions.clone();
        let actor = SearchActor::start_with_worker(move |_| {
            worker_executions.fetch_add(1, Ordering::Relaxed);
            Ok(json!({"results": [], "unavailable": []}))
        });
        for _ in 0..3 {
            let (response, receiver) = oneshot::channel();
            drop(receiver);
            actor
                .sender
                .try_send(SearchCommand {
                    kind: SearchKind::Text {
                        text: "cancelled".to_string(),
                        strategy: image_index::QueryStrategy::Fast,
                        limit: 1,
                    },
                    response,
                })
                .unwrap();
        }
        for _ in 0..100 {
            if actor.metrics.cancelled.load(Ordering::Relaxed) == 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(actor.metrics.cancelled.load(Ordering::Relaxed), 3);
        assert_eq!(executions.load(Ordering::Relaxed), 0);
    }
}

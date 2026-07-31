use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use crossbeam_channel::{bounded, Receiver, Sender};
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::image_index;
use crate::image_insights;
use crate::msg3_log_service_assets::{candidate_path_groups_with_resolver, AssetResolver};
use crate::msg3_log_service_rich::attach_mmp_items_to_multi_msg;
use crate::msg3_log_service_tables::{quote_ident, table_names};
use crate::msg3_parser;

const IDENTITY_SAMPLE_BYTES: usize = 64 * 1024;
const ASSET_ID_CACHE_CAPACITY: usize = 200_000;
const LINKER_VERSION: i64 = 2;
pub const DEFAULT_LINK_BATCH_SIZE: usize = 8_000;

pub fn recommended_link_workers() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(4)
        .clamp(4, 12)
}

#[derive(Clone, Debug)]
pub struct LinkChatOptions {
    pub root: PathBuf,
    pub account: String,
    pub manifest_path: Option<PathBuf>,
    pub chat_db: PathBuf,
    pub workers: usize,
    pub batch_size: usize,
    pub force: bool,
    /// Optional pilot cap, primarily for real-data throughput estimates.
    pub max_rows: Option<usize>,
    /// Cooperative cancellation checked between committed batches.
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LinkTableReport {
    pub table: String,
    pub rebuilt: bool,
    pub resumed_from_rowid: i64,
    pub last_rowid: i64,
    pub max_rowid: i64,
    pub rows_scanned: usize,
    pub occurrences_linked: usize,
    pub unresolved_assets: usize,
    pub parse_errors: usize,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LinkChatReport {
    pub account: String,
    pub manifest: String,
    pub chat_db: String,
    pub source_identity: String,
    pub workers: usize,
    pub batch_size: usize,
    pub force: bool,
    pub cancelled: bool,
    pub rows_scanned: usize,
    pub occurrences_linked: usize,
    pub parse_errors: usize,
    pub elapsed_ms: u64,
    pub rows_per_second: f64,
    pub estimated_source_rows: i64,
    pub estimated_rows_pending_before: i64,
    pub estimated_rows_pending_after: i64,
    pub estimated_seconds_remaining: f64,
    pub tables: Vec<LinkTableReport>,
}

#[derive(Clone)]
struct Job {
    table: String,
    rowid: i64,
    message_time: i64,
    sender_uin: String,
    msg_content: Vec<u8>,
    info: Vec<u8>,
    mmp_only: bool,
}

struct JobResult {
    table: String,
    rowid: i64,
    message_time: i64,
    sender_uin: String,
    candidate_groups: Vec<Vec<String>>,
    parse_error: bool,
}

#[derive(Clone)]
struct SourceIdentity {
    canonical_path: String,
    identity: String,
    size: i64,
    mtime_unix: i64,
}

#[derive(Default)]
struct ScanState {
    identity: String,
    size: i64,
    last_rowid: i64,
    linker_version: i64,
    scan_mode: String,
}

pub fn link_chat(options: LinkChatOptions) -> anyhow::Result<LinkChatReport> {
    if !options.chat_db.is_file() {
        anyhow::bail!(
            "prepared chat database is missing: {}",
            options.chat_db.display()
        );
    }
    let manifest = options
        .manifest_path
        .clone()
        .unwrap_or_else(|| image_index::default_manifest_path(&options.root, &options.account));
    if !manifest.is_file() {
        anyhow::bail!("image index manifest is missing: {}", manifest.display());
    }
    image_index::prepare_web_manifest_path(&manifest)?;
    let started = Instant::now();
    let workers = options.workers.max(1).min(64);
    let batch_size = options.batch_size.max(1);
    let source = source_identity(&options.chat_db)?;
    let chat = Connection::open_with_flags(
        &options.chat_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut manifest_con = Connection::open(&manifest)?;
    manifest_con.execute_batch("pragma busy_timeout=5000; pragma journal_mode=WAL;")?;
    let tables = table_names(&chat)?;
    materialize_scan_states(&chat, &mut manifest_con, &source, &tables)?;
    let (estimated_source_rows, estimated_rows_pending_before) =
        estimate_source_work(&chat, &manifest_con, &source, &tables, options.force)?;
    let (job_tx, job_rx) = bounded::<Job>(batch_size.saturating_mul(2).max(workers));
    let (result_tx, result_rx) = bounded::<JobResult>(batch_size.saturating_mul(2).max(workers));
    let mut reports = Vec::new();
    let mut remaining = options.max_rows.unwrap_or(usize::MAX);
    let mut asset_id_cache = HashMap::<String, Option<i64>>::new();
    let lookup_root = options
        .root
        .canonicalize()
        .unwrap_or_else(|_| options.root.clone());

    std::thread::scope(|scope| -> anyhow::Result<()> {
        for _ in 0..workers {
            let jobs = job_rx.clone();
            let results = result_tx.clone();
            let root = &options.root;
            let account = &options.account;
            scope.spawn(move || worker_loop(root, account, jobs, results));
        }
        drop(result_tx);
        for table in &tables {
            if remaining == 0 || is_cancelled(options.cancel.as_ref()) {
                break;
            }
            let report = link_table(
                &chat,
                &mut manifest_con,
                &lookup_root,
                &source,
                &table,
                batch_size,
                options.force,
                options.cancel.as_ref(),
                &mut remaining,
                &job_tx,
                &result_rx,
                &mut asset_id_cache,
                started,
            )?;
            reports.push(report);
        }
        drop(job_tx);
        Ok(())
    })?;

    let rows_scanned = reports.iter().map(|table| table.rows_scanned).sum();
    let occurrences_linked = reports.iter().map(|table| table.occurrences_linked).sum();
    let parse_errors = reports.iter().map(|table| table.parse_errors).sum();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let rows_per_second = if elapsed_ms == 0 {
        rows_scanned as f64
    } else {
        rows_scanned as f64 / (elapsed_ms as f64 / 1_000.0)
    };
    let (_, estimated_rows_pending_after) =
        estimate_source_work(&chat, &manifest_con, &source, &tables, false)?;
    let estimated_seconds_remaining = if rows_per_second > 0.0 {
        estimated_rows_pending_after as f64 / rows_per_second
    } else {
        0.0
    };
    image_index::refresh_overview_reference_cache(&mut manifest_con)?;
    let cancelled = is_cancelled(options.cancel.as_ref());
    Ok(LinkChatReport {
        account: options.account,
        manifest: manifest.display().to_string(),
        chat_db: options.chat_db.display().to_string(),
        source_identity: source.identity,
        workers,
        batch_size,
        force: options.force,
        cancelled,
        rows_scanned,
        occurrences_linked,
        parse_errors,
        elapsed_ms,
        rows_per_second,
        estimated_source_rows,
        estimated_rows_pending_before,
        estimated_rows_pending_after,
        estimated_seconds_remaining,
        tables: reports,
    })
}

#[allow(clippy::too_many_arguments)]
fn link_table(
    chat: &Connection,
    manifest: &mut Connection,
    root: &Path,
    source: &SourceIdentity,
    table: &str,
    batch_size: usize,
    force: bool,
    cancel: Option<&Arc<AtomicBool>>,
    remaining: &mut usize,
    jobs: &Sender<Job>,
    results: &Receiver<JobResult>,
    asset_id_cache: &mut HashMap<String, Option<i64>>,
    run_started: Instant,
) -> anyhow::Result<LinkTableReport> {
    let quoted = quote_ident(table);
    let max_rowid = chat.query_row(
        &format!("select coalesce(max(rowid),0) from {quoted}"),
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let previous = load_scan_state(manifest, table)?.unwrap_or_default();
    let rebuilt = force
        || (!previous.identity.is_empty() && previous.identity != source.identity)
        || previous.size > source.size
        || previous.last_rowid > max_rowid;
    let starts_linker_upgrade = !rebuilt && previous.linker_version < LINKER_VERSION;
    let mmp_only = !rebuilt && (starts_linker_upgrade || previous.scan_mode == "mmp");
    if rebuilt || starts_linker_upgrade {
        let tx = manifest.transaction()?;
        if rebuilt {
            let removed = tx.execute(
                "delete from image_asset_occurrences where conversation_table=?1",
                [table],
            )?;
            if removed > 0 {
                invalidate_popularity_build(&tx, &current_timestamp())?;
            }
        }
        tx.execute(
            "delete from image_occurrence_scan_state where conversation_table=?1",
            [table],
        )?;
        tx.commit()?;
    }
    let start_rowid = if rebuilt || starts_linker_upgrade {
        0
    } else {
        previous.last_rowid
    };
    let mut last_rowid = start_rowid;
    let mut report = LinkTableReport {
        table: table.to_string(),
        rebuilt,
        resumed_from_rowid: start_rowid,
        max_rowid,
        ..LinkTableReport::default()
    };
    while last_rowid < max_rowid && *remaining > 0 && !is_cancelled(cancel) {
        let take = batch_size.min(*remaining);
        // Materialize the batch before dispatching it. If SQLite encounters a
        // corrupt source page halfway through iteration, no orphan worker
        // results are left in the channel when we recover below.
        let batch = (|| -> rusqlite::Result<Vec<(i64, Vec<u8>, i64, String, Vec<u8>)>> {
            let mut stmt = chat.prepare(&format!(
                "select rowid,MsgContent,Time,SenderUin from {quoted} where rowid>?1 order by rowid limit ?2"
            ))?;
            let mut rows = stmt.query(params![last_rowid, take as i64])?;
            let mut messages = Vec::with_capacity(take);
            while let Some(row) = rows.next()? {
                messages.push((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1).unwrap_or_default(),
                    integer_value(row.get_ref(2)?),
                    text_value(row.get_ref(3)?),
                ));
            }
            drop(rows);
            drop(stmt);
            // Info can be much larger than MsgContent. Fetch it only for the
            // tiny subset whose framing can contain a forwarded-record node.
            let mut info_stmt =
                chat.prepare(&format!("select Info from {quoted} where rowid=?1"))?;
            let mut batch = Vec::with_capacity(messages.len());
            for (rowid, msg_content, message_time, sender_uin) in messages {
                let info = if msg3_parser::msgcontent_maybe_contains_multi_msg(&msg_content) {
                    info_stmt.query_row([rowid], |row| {
                        Ok(row.get::<_, Vec<u8>>(0).unwrap_or_default())
                    })?
                } else {
                    Vec::new()
                };
                batch.push((rowid, msg_content, message_time, sender_uin, info));
            }
            Ok(batch)
        })();
        let batch = match batch {
            Ok(batch) => batch,
            Err(error) if sqlite_source_is_corrupt(&error) => {
                let skipped_from = last_rowid.saturating_add(1);
                last_rowid = last_rowid.saturating_add(take as i64).min(max_rowid);
                report.parse_errors += 1;
                *remaining = remaining.saturating_sub(take);
                commit_scan_checkpoint(
                    manifest,
                    source,
                    table,
                    last_rowid,
                    max_rowid,
                    0,
                    0,
                    if mmp_only { "mmp" } else { "full" },
                )?;
                eprintln!(
                    "{}",
                    json!({
                        "event": "image_index_link_corrupt_range_skipped",
                        "table": table,
                        "first_rowid": skipped_from,
                        "last_rowid": last_rowid,
                        "error": error.to_string(),
                    })
                );
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let count = batch.len();
        let mut batch_last = last_rowid;
        let mut dispatched = 0usize;
        for (rowid, msg_content, message_time, sender_uin, info) in batch {
            batch_last = rowid;
            jobs.send(Job {
                table: table.to_string(),
                rowid,
                message_time,
                sender_uin,
                msg_content,
                info,
                mmp_only,
            })?;
            dispatched += 1;
        }
        if count == 0 {
            break;
        }
        let mut batch_results = Vec::with_capacity(dispatched);
        for _ in 0..dispatched {
            batch_results.push(results.recv()?);
        }
        batch_results.sort_by_key(|result| result.rowid);
        let mut resolved_results = Vec::with_capacity(batch_results.len());
        for result in batch_results {
            if result.table != table {
                anyhow::bail!("link worker returned a result for the wrong table");
            }
            if result.parse_error {
                report.parse_errors += 1;
            }
            let mut matched = HashSet::new();
            for candidates in result.candidate_groups {
                let mut resolved = false;
                for path in candidates {
                    if let Some(asset_id) =
                        resolve_asset_id_cached(manifest, root, &path, asset_id_cache)?
                    {
                        matched.insert(asset_id);
                        resolved = true;
                        break;
                    }
                }
                if !resolved {
                    report.unresolved_assets += 1;
                }
            }
            resolved_results.push((
                result.rowid,
                result.message_time,
                result.sender_uin,
                matched,
            ));
        }
        let tx = manifest.transaction()?;
        let linked_at = current_timestamp();
        let conversation_type = crate::msg3_log_service_tables::split_table(table).0;
        let mut batch_occurrences = 0usize;
        let mut batch_fact_updates = 0usize;
        for (rowid, message_time, sender_uin, matched) in &resolved_results {
            let message_date = image_insights::message_date(*message_time);
            if mmp_only {
                batch_fact_updates += tx.execute(
                    r#"update image_asset_occurrences
                       set message_time=?3,message_date=?4,sender_uin=?5,
                           conversation_type=?6,facts_updated_at=?7
                       where conversation_table=?1 and message_rowid=?2"#,
                    params![
                        table,
                        rowid,
                        message_time,
                        message_date,
                        sender_uin,
                        conversation_type,
                        linked_at,
                    ],
                )?;
            }
            for asset_id in matched {
                batch_occurrences += tx.execute(
                    r#"insert or ignore into image_asset_occurrences(
                           asset_id,conversation_table,message_rowid,linked_at,
                           message_time,message_date,sender_uin,conversation_type,
                           facts_updated_at
                       ) values(?1,?2,?3,?4,?5,?6,?7,?8,?4)
                       "#,
                    params![
                        asset_id,
                        table,
                        rowid,
                        linked_at,
                        message_time,
                        message_date,
                        sender_uin,
                        conversation_type,
                    ],
                )?;
            }
        }
        if batch_occurrences > 0 || batch_fact_updates > 0 {
            invalidate_popularity_build(&tx, &linked_at)?;
        }
        report.rows_scanned += count;
        report.occurrences_linked += batch_occurrences;
        last_rowid = batch_last;
        *remaining = remaining.saturating_sub(count);
        let complete = last_rowid >= max_rowid;
        tx.execute(
            r#"insert into image_occurrence_scan_state(
                conversation_table,source_db_path,source_db_identity,source_db_size,
                source_db_mtime_unix,last_rowid,max_rowid,rows_scanned,
                occurrences_linked,completed,linker_version,scan_mode,updated_at
            ) values(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
            on conflict(conversation_table) do update set
                source_db_path=excluded.source_db_path,
                source_db_identity=excluded.source_db_identity,
                source_db_size=excluded.source_db_size,
                source_db_mtime_unix=excluded.source_db_mtime_unix,
                last_rowid=excluded.last_rowid,
                max_rowid=excluded.max_rowid,
                rows_scanned=image_occurrence_scan_state.rows_scanned+excluded.rows_scanned,
                occurrences_linked=image_occurrence_scan_state.occurrences_linked+excluded.occurrences_linked,
                completed=excluded.completed,
                linker_version=excluded.linker_version,
                scan_mode=excluded.scan_mode,
                updated_at=excluded.updated_at"#,
            params![
                table,
                source.canonical_path,
                source.identity,
                source.size,
                source.mtime_unix,
                last_rowid,
                max_rowid,
                count as i64,
                batch_occurrences as i64,
                complete,
                LINKER_VERSION,
                if mmp_only { "mmp" } else { "full" },
                linked_at,
            ],
        )?;
        tx.commit()?;
        let elapsed = run_started.elapsed().as_secs_f64().max(0.001);
        let rate = report.rows_scanned as f64 / elapsed;
        let pending = max_rowid.saturating_sub(last_rowid) as f64;
        let eta_seconds = if rate > 0.0 { pending / rate } else { 0.0 };
        eprintln!(
            "{}",
            json!({
                "event": "image_index_link_progress",
                "table": table,
                "last_rowid": last_rowid,
                "max_rowid": max_rowid,
                "rows_scanned": report.rows_scanned,
                "rows_per_second": rate,
                "eta_seconds": eta_seconds,
            })
        );
    }
    report.last_rowid = last_rowid;
    report.complete = last_rowid >= max_rowid;
    Ok(report)
}

fn invalidate_popularity_build(con: &Connection, refreshed_at: &str) -> anyhow::Result<()> {
    image_index::mark_occurrence_content_changed(con)?;
    con.execute(
        r#"insert into image_popularity_build_state(
               id,schema_version,occurrence_count,fact_count,cube_row_count,
               summary_ready,refreshed_at
           ) values(1,2,0,0,0,0,?1)
           on conflict(id) do update set
               schema_version=excluded.schema_version,
               occurrence_count=0,
               fact_count=0,
               cube_row_count=0,
               summary_ready=0,
               refreshed_at=excluded.refreshed_at"#,
        [refreshed_at],
    )?;
    Ok(())
}

fn is_cancelled(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel
        .map(|flag| flag.load(Ordering::Relaxed))
        .unwrap_or(false)
}

fn sqlite_source_is_corrupt(error: &rusqlite::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("malformed") || message.contains("corrupt")
}

fn commit_scan_checkpoint(
    manifest: &mut Connection,
    source: &SourceIdentity,
    table: &str,
    last_rowid: i64,
    max_rowid: i64,
    rows_scanned: usize,
    occurrences_linked: usize,
    scan_mode: &str,
) -> anyhow::Result<()> {
    manifest.execute(
        r#"insert into image_occurrence_scan_state(
            conversation_table,source_db_path,source_db_identity,source_db_size,
            source_db_mtime_unix,last_rowid,max_rowid,rows_scanned,
            occurrences_linked,completed,linker_version,scan_mode,updated_at
        ) values(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
        on conflict(conversation_table) do update set
            source_db_path=excluded.source_db_path,
            source_db_identity=excluded.source_db_identity,
            source_db_size=excluded.source_db_size,
            source_db_mtime_unix=excluded.source_db_mtime_unix,
            last_rowid=excluded.last_rowid,
            max_rowid=excluded.max_rowid,
            rows_scanned=image_occurrence_scan_state.rows_scanned+excluded.rows_scanned,
            occurrences_linked=image_occurrence_scan_state.occurrences_linked+excluded.occurrences_linked,
            completed=excluded.completed,
            linker_version=excluded.linker_version,
            scan_mode=excluded.scan_mode,
            updated_at=excluded.updated_at"#,
        params![
            table,
            source.canonical_path,
            source.identity,
            source.size,
            source.mtime_unix,
            last_rowid,
            max_rowid,
            rows_scanned as i64,
            occurrences_linked as i64,
            last_rowid >= max_rowid,
            LINKER_VERSION,
            scan_mode,
            current_timestamp(),
        ],
    )?;
    Ok(())
}

fn worker_loop(root: &Path, account: &str, jobs: Receiver<Job>, results: Sender<JobResult>) {
    let mut resolver = AssetResolver::new(root, account);
    while let Ok(job) = jobs.recv() {
        if job.mmp_only && !msg3_parser::msgcontent_maybe_contains_multi_msg(&job.msg_content) {
            if results
                .send(JobResult {
                    table: job.table,
                    rowid: job.rowid,
                    message_time: job.message_time,
                    sender_uin: job.sender_uin,
                    candidate_groups: Vec::new(),
                    parse_error: false,
                })
                .is_err()
            {
                break;
            }
            continue;
        }
        let rich_nodes_json = msg3_parser::parse_msgcontent_rich_nodes_json(&job.msg_content);
        let parsed = serde_json::from_str::<Value>(&rich_nodes_json);
        let (candidate_groups, parse_error) = match parsed {
            Ok(mut nodes) => {
                if contains_multi_msg_node(&nodes) && !job.info.is_empty() {
                    let mmp_items = serde_json::from_str::<Value>(
                        &msg3_parser::parse_info_mmp_items_json(&job.info),
                    )
                    .unwrap_or_else(|_| Value::Array(Vec::new()));
                    attach_mmp_items_to_multi_msg(&mut nodes, &mmp_items, account);
                }
                let groups = candidate_path_groups_with_resolver(&mut resolver, &nodes);
                (groups, false)
            }
            Err(_) => (Vec::new(), true),
        };
        if results
            .send(JobResult {
                table: job.table,
                rowid: job.rowid,
                message_time: job.message_time,
                sender_uin: job.sender_uin,
                candidate_groups,
                parse_error,
            })
            .is_err()
        {
            break;
        }
    }
}

fn contains_multi_msg_node(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_multi_msg_node),
        Value::Object(object) => {
            object.get("type").and_then(Value::as_str) == Some("multi_msg")
                || object.values().any(contains_multi_msg_node)
        }
        _ => false,
    }
}

fn integer_value(value: ValueRef<'_>) -> i64 {
    match value {
        ValueRef::Integer(value) => value,
        ValueRef::Real(value) => value as i64,
        ValueRef::Text(value) => String::from_utf8_lossy(value).parse().unwrap_or(0),
        _ => 0,
    }
}

fn text_value(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Integer(value) => value.to_string(),
        ValueRef::Real(value) => value.to_string(),
        ValueRef::Text(value) | ValueRef::Blob(value) => {
            String::from_utf8_lossy(value).into_owned()
        }
        ValueRef::Null => String::new(),
    }
}

fn resolve_asset_id(con: &Connection, root: &Path, path: &str) -> anyhow::Result<Option<i64>> {
    let mut stmt = con.prepare_cached("select id from image_assets where path=?1 and stale=0")?;
    for candidate in crate::image_index_web::manifest_path_candidates(root, path) {
        if let Some(id) = stmt.query_row([candidate], |row| row.get(0)).optional()? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn resolve_asset_id_cached(
    con: &Connection,
    root: &Path,
    path: &str,
    cache: &mut HashMap<String, Option<i64>>,
) -> anyhow::Result<Option<i64>> {
    if let Some(id) = cache.get(path) {
        return Ok(*id);
    }
    let id = resolve_asset_id(con, root, path)?;
    if cache.len() >= ASSET_ID_CACHE_CAPACITY {
        cache.clear();
    }
    cache.insert(path.to_string(), id);
    Ok(id)
}

fn load_scan_state(con: &Connection, table: &str) -> anyhow::Result<Option<ScanState>> {
    Ok(con
        .query_row(
            "select source_db_identity,source_db_size,last_rowid,linker_version,scan_mode from image_occurrence_scan_state where conversation_table=?1",
            [table],
            |row| {
                Ok(ScanState {
                    identity: row.get(0)?,
                    size: row.get(1)?,
                    last_rowid: row.get(2)?,
                    linker_version: row.get(3)?,
                    scan_mode: row.get(4)?,
                    ..ScanState::default()
                })
            },
        )
        .optional()?)
}

/// Register every source table before scanning begins so maintenance progress
/// has a stable denominator. Previously a table appeared in scan state only
/// when the worker reached it, making the UI sit near 100% while the total kept
/// growing one table at a time.
fn materialize_scan_states(
    chat: &Connection,
    manifest: &mut Connection,
    source: &SourceIdentity,
    tables: &[String],
) -> anyhow::Result<()> {
    let updated_at = current_timestamp();
    let tx = manifest.transaction()?;
    let current_tables = tables.iter().map(String::as_str).collect::<HashSet<_>>();
    let stale_tables = {
        let mut statement =
            tx.prepare("select conversation_table from image_occurrence_scan_state")?;
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        tables
            .into_iter()
            .filter(|table| !current_tables.contains(table.as_str()))
            .collect::<Vec<_>>()
    };
    let mut removed_occurrences = 0_usize;
    let mut removed_derived_rows = 0_usize;
    for table in stale_tables {
        removed_occurrences += tx.execute(
            "delete from image_asset_occurrences where conversation_table=?1",
            [&table],
        )?;
        removed_derived_rows += tx.execute(
            "delete from image_popularity_cube where conversation_table=?1",
            [&table],
        )?;
        removed_derived_rows += tx.execute(
            "delete from image_popularity_daily_conversation where conversation_table=?1",
            [&table],
        )?;
        tx.execute(
            "delete from image_popularity_analysis_state where conversation_table=?1",
            [&table],
        )?;
        tx.execute(
            "delete from image_occurrence_scan_state where conversation_table=?1",
            [&table],
        )?;
    }
    if removed_occurrences > 0 || removed_derived_rows > 0 {
        tx.execute("delete from image_popularity_summary_cache", [])?;
        invalidate_popularity_build(&tx, &updated_at)?;
    }
    let mut upsert = tx.prepare(
        r#"insert into image_occurrence_scan_state(
            conversation_table,source_db_path,source_db_identity,source_db_size,
            source_db_mtime_unix,last_rowid,max_rowid,rows_scanned,
            occurrences_linked,completed,linker_version,scan_mode,updated_at
        ) values(?1,?2,?3,?4,?5,0,?6,0,0,?7,?8,'full',?9)
        on conflict(conversation_table) do update set
            source_db_path=excluded.source_db_path,
            source_db_size=excluded.source_db_size,
            source_db_mtime_unix=excluded.source_db_mtime_unix,
            max_rowid=excluded.max_rowid,
            completed=case
                when image_occurrence_scan_state.last_rowid>=excluded.max_rowid then 1
                else 0
            end
        where image_occurrence_scan_state.source_db_identity=excluded.source_db_identity
          and image_occurrence_scan_state.source_db_size<=excluded.source_db_size"#,
    )?;
    for table in tables {
        let max_rowid = chat.query_row(
            &format!("select coalesce(max(rowid),0) from {}", quote_ident(table)),
            [],
            |row| row.get::<_, i64>(0),
        )?;
        upsert.execute(params![
            table,
            source.canonical_path,
            source.identity,
            source.size,
            source.mtime_unix,
            max_rowid,
            max_rowid == 0,
            LINKER_VERSION,
            updated_at,
        ])?;
    }
    drop(upsert);
    tx.commit()?;
    Ok(())
}

fn estimate_source_work(
    chat: &Connection,
    manifest: &Connection,
    source: &SourceIdentity,
    tables: &[String],
    force: bool,
) -> anyhow::Result<(i64, i64)> {
    let mut source_rows = 0_i64;
    let mut pending_rows = 0_i64;
    for table in tables {
        let max_rowid = chat.query_row(
            &format!("select coalesce(max(rowid),0) from {}", quote_ident(table)),
            [],
            |row| row.get::<_, i64>(0),
        )?;
        source_rows = source_rows.saturating_add(max_rowid);
        let previous = load_scan_state(manifest, table)?.unwrap_or_default();
        let rebuilt = force
            || (!previous.identity.is_empty() && previous.identity != source.identity)
            || previous.size > source.size
            || previous.last_rowid > max_rowid;
        let start_rowid = if rebuilt || previous.linker_version < LINKER_VERSION {
            0
        } else {
            previous.last_rowid
        };
        pending_rows = pending_rows.saturating_add(max_rowid.saturating_sub(start_rowid));
    }
    Ok((source_rows, pending_rows))
}

fn source_identity(path: &Path) -> anyhow::Result<SourceIdentity> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let metadata = canonical.metadata()?;
    let size = metadata.len() as i64;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    let mtime_unix = modified
        .as_ref()
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let mut digest = Sha256::new();
    update_identity_digest(&mut digest, b"main", &canonical, &metadata)?;
    let mut wal_name = canonical.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    if let Ok(wal_metadata) = wal_path.metadata() {
        update_identity_digest(&mut digest, b"wal", &wal_path, &wal_metadata)?;
    } else {
        digest.update(b"wal:missing");
    }
    Ok(SourceIdentity {
        canonical_path: canonical.display().to_string(),
        identity: format!("sha256-sampled-v2:{:x}", digest.finalize()),
        size,
        mtime_unix,
    })
}

fn update_identity_digest(
    digest: &mut Sha256,
    label: &[u8],
    path: &Path,
    metadata: &std::fs::Metadata,
) -> anyhow::Result<()> {
    digest.update(label);
    digest.update(metadata.len().to_le_bytes());
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    digest.update(modified_nanos.to_le_bytes());

    let mut file = File::open(path)?;
    let sample_len = metadata.len().min(IDENTITY_SAMPLE_BYTES as u64);
    let mut sample = vec![0u8; sample_len as usize];
    file.read_exact(&mut sample)?;
    digest.update(&sample);
    if metadata.len() > sample_len {
        let middle = (metadata.len() / 2).saturating_sub(sample_len / 2);
        file.seek(SeekFrom::Start(middle))?;
        file.read_exact(&mut sample)?;
        digest.update(&sample);
        file.seek(SeekFrom::End(-(sample_len as i64)))?;
        file.read_exact(&mut sample)?;
        digest.update(&sample);
    }
    Ok(())
}

#[cfg(test)]
fn path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn current_timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampled_identity_changes_when_content_changes() {
        let path = std::env::temp_dir().join(format!(
            "qq-analyzer-link-identity-{}.db",
            std::process::id()
        ));
        std::fs::write(&path, vec![1u8; 400_000]).unwrap();
        let first = source_identity(&path).unwrap();
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(200_000)).unwrap();
        std::io::Write::write_all(&mut file, &[2u8; 4096]).unwrap();
        file.sync_all().unwrap();
        let second = source_identity(&path).unwrap();
        assert_eq!(first.size, second.size);
        assert_ne!(first.identity, second.identity);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sampled_identity_includes_wal_changes() {
        let path = std::env::temp_dir().join(format!(
            "qq-analyzer-link-wal-identity-{}.db",
            std::process::id()
        ));
        std::fs::write(&path, vec![1u8; 8192]).unwrap();
        let first = source_identity(&path).unwrap();
        let mut wal_name = path.as_os_str().to_os_string();
        wal_name.push("-wal");
        let wal_path = PathBuf::from(wal_name);
        std::fs::write(&wal_path, vec![3u8; 8192]).unwrap();
        let second = source_identity(&path).unwrap();
        assert_ne!(first.identity, second.identity);
        let _ = std::fs::remove_file(wal_path);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn path_keys_normalize_separators() {
        assert_eq!(path_key("a\\b\\c.png"), path_key("a/b/c.png"));
    }

    #[test]
    fn path_candidates_bridge_wsl_and_extended_windows_paths() {
        let windows = r"\\?\D:\QQData\1234567890\Image\a.jpg";
        let candidates = crate::image_index_web::manifest_path_candidates(
            Path::new("/workspace"),
            "/mnt/d/QQData/1234567890/Image/a.jpg",
        );
        assert!(candidates.iter().any(|candidate| candidate == windows));
        assert!(
            crate::image_index_web::manifest_path_candidates(Path::new("/workspace"), windows)
                .iter()
                .any(|candidate| candidate == r"D:\QQData\1234567890\Image\a.jpg")
        );
    }

    #[test]
    fn linker_resumes_force_rebuilds_and_detects_replaced_source() {
        let root = std::env::temp_dir().join(format!(
            "qq-analyzer-link-resume-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let account = "10001";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        con.execute(
            r#"insert into image_popularity_build_state(
                   id,schema_version,occurrence_count,fact_count,cube_row_count,
                   summary_ready,refreshed_at
               ) values(1,2,0,0,0,1,'stable-generation')"#,
            [],
        )
        .unwrap();
        drop(con);
        let chat_db = root.join("prepared-chat.sqlite");
        write_chat_fixture(&chat_db, 2, 7);
        let options = || LinkChatOptions {
            root: root.clone(),
            account: account.to_string(),
            manifest_path: Some(manifest.clone()),
            chat_db: chat_db.clone(),
            workers: 2,
            batch_size: 1,
            force: false,
            max_rows: None,
            cancel: None,
        };
        let first = link_chat(options()).unwrap();
        assert_eq!(first.rows_scanned, 2);
        assert!(!first.tables[0].rebuilt);
        let con = Connection::open(&manifest).unwrap();
        let (ready, generation): (bool, String) = con
            .query_row(
                "select summary_ready,refreshed_at from image_popularity_build_state where id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(ready);
        assert_eq!(generation, "stable-generation");
        drop(con);
        let resumed = link_chat(options()).unwrap();
        assert_eq!(resumed.rows_scanned, 0);
        assert_eq!(resumed.tables[0].resumed_from_rowid, 2);

        let mut forced_options = options();
        forced_options.force = true;
        let forced = link_chat(forced_options).unwrap();
        assert_eq!(forced.rows_scanned, 2);
        assert!(forced.tables[0].rebuilt);

        std::fs::remove_file(&chat_db).unwrap();
        write_chat_fixture(&chat_db, 1, 99);
        let replaced = link_chat(options()).unwrap();
        assert_eq!(replaced.rows_scanned, 1);
        assert!(replaced.tables[0].rebuilt);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn linker_materializes_unreached_tables_for_stable_progress() {
        let root = std::env::temp_dir().join(format!(
            "qq-analyzer-link-progress-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let account = "10001";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let chat_db = root.join("prepared-chat.sqlite");
        write_chat_fixture(&chat_db, 2, 7);
        let chat = Connection::open(&chat_db).unwrap();
        chat.execute(
            "create table group_2(Time integer,Rand integer,SenderUin integer,MsgContent blob,Info blob)",
            [],
        )
        .unwrap();
        chat.execute("insert into group_2 values(1700000000,1,7,x'',x'')", [])
            .unwrap();
        drop(chat);

        let report = link_chat(LinkChatOptions {
            root: root.clone(),
            account: account.to_string(),
            manifest_path: Some(manifest.clone()),
            chat_db,
            workers: 1,
            batch_size: 1,
            force: false,
            max_rows: Some(1),
            cancel: None,
        })
        .unwrap();
        assert_eq!(report.rows_scanned, 1);
        let con = Connection::open(manifest).unwrap();
        let (tables, total): (i64, i64) = con
            .query_row(
                "select count(*),sum(max_rowid) from image_occurrence_scan_state",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(tables, 2);
        assert_eq!(total, 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn materializing_scan_states_removes_deleted_conversation_data() {
        let root = std::env::temp_dir().join(format!(
            "qq-analyzer-link-deleted-table-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let manifest_path = root.join("manifest.sqlite");
        drop(Connection::open(&manifest_path).unwrap());
        image_index::prepare_web_manifest_path(&manifest_path).unwrap();
        let mut manifest = Connection::open(&manifest_path).unwrap();
        manifest
            .execute_batch(
                r#"insert into image_occurrence_scan_state(
                       conversation_table,source_db_path,source_db_identity,
                       source_db_size,source_db_mtime_unix,last_rowid,max_rowid,
                       rows_scanned,occurrences_linked,completed,linker_version,
                       scan_mode,updated_at
                   ) values(
                       'group_removed','old.sqlite','old',1,1,1,1,1,1,1,2,
                       'full','old'
                   );
                   insert into image_asset_occurrences(
                       asset_id,conversation_table,message_rowid
                   ) values(1,'group_removed',1);
                   insert into image_popularity_analysis_state(
                       conversation_table,last_message_rowid,max_message_rowid,
                       occurrence_count,fact_count,completed,updated_at
                   ) values('group_removed',1,1,1,1,1,'old');
                   insert into image_popularity_cube(
                       representative_asset_id,message_date,sender_uin,
                       conversation_table,conversation_type,reference_count
                   ) values(1,'2024-01-01','sender','group_removed','group',1);"#,
            )
            .unwrap();

        let chat_path = root.join("chat.sqlite");
        let chat = Connection::open(&chat_path).unwrap();
        chat.execute(
            "create table group_kept(Time integer,Rand integer,SenderUin integer,MsgContent blob,Info blob)",
            [],
        )
        .unwrap();
        let source = source_identity(&chat_path).unwrap();
        let tables = table_names(&chat).unwrap();
        materialize_scan_states(&chat, &mut manifest, &source, &tables).unwrap();

        for table in [
            "image_occurrence_scan_state",
            "image_asset_occurrences",
            "image_popularity_analysis_state",
            "image_popularity_cube",
        ] {
            let remaining: i64 = manifest
                .query_row(
                    &format!(
                        "select count(*) from {table} where conversation_table='group_removed'"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(remaining, 0, "{table} retained deleted conversation data");
        }
        let kept_state: i64 = manifest
            .query_row(
                "select count(*) from image_occurrence_scan_state where conversation_table='group_kept'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept_state, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn linker_writes_message_facts_with_the_occurrence_in_one_scan() {
        let root = std::env::temp_dir().join(format!(
            "qq-analyzer-link-facts-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let account = "10001";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let indexed_path = root.join(account).join("Image").join("linked-image.jpg");
        let con = Connection::open(&manifest).unwrap();
        con.execute(
            "insert into image_assets(id,path,stale) values(1,?1,0)",
            [indexed_path.to_string_lossy().as_ref()],
        )
        .unwrap();
        drop(con);

        let token = "UserDataImage:linked-image.jpg"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let mut payload = vec![2, token.len() as u8, 0];
        payload.extend_from_slice(&token);
        let mut msg_content = vec![3, payload.len() as u8, 0];
        msg_content.extend_from_slice(&payload);
        let chat_db = root.join("prepared-chat.sqlite");
        let chat = Connection::open(&chat_db).unwrap();
        chat.execute(
            "create table group_1(Time integer,Rand integer,SenderUin text,MsgContent blob,Info blob)",
            [],
        )
        .unwrap();
        chat.execute(
            "insert into group_1 values(?1,1,?2,?3,x'')",
            params![1_704_067_200_i64, "sender-text", msg_content],
        )
        .unwrap();
        drop(chat);

        let report = link_chat(LinkChatOptions {
            root: root.clone(),
            account: account.to_string(),
            manifest_path: Some(manifest.clone()),
            chat_db,
            workers: 1,
            batch_size: 1,
            force: false,
            max_rows: None,
            cancel: None,
        })
        .unwrap();
        assert_eq!(report.rows_scanned, 1);
        assert_eq!(report.occurrences_linked, 1);
        let con = Connection::open(&manifest).unwrap();
        let facts: (i64, String, String, String, bool) = con
            .query_row(
                r#"select message_time,message_date,sender_uin,
                          conversation_type,facts_updated_at<>''
                   from image_asset_occurrences
                   where asset_id=1 and conversation_table='group_1'
                     and message_rowid=1"#,
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            facts,
            (
                1_704_067_200,
                "2024-01-01".to_string(),
                "sender-text".to_string(),
                "group".to_string(),
                true
            )
        );
        let legacy_table: i64 = con
            .query_row(
                "select count(*) from sqlite_master where type='table' and name='image_occurrence_message_facts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_table, 0);
        let ready: bool = con
            .query_row(
                "select summary_ready from image_popularity_build_state where id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!ready);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn linker_upgrade_rescans_without_deleting_existing_occurrences() {
        let root = std::env::temp_dir().join(format!(
            "qq-analyzer-link-upgrade-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let account = "10001";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let chat_db = root.join("prepared-chat.sqlite");
        write_chat_fixture(&chat_db, 2, 7);
        let options = || LinkChatOptions {
            root: root.clone(),
            account: account.to_string(),
            manifest_path: Some(manifest.clone()),
            chat_db: chat_db.clone(),
            workers: 1,
            batch_size: 2,
            force: false,
            max_rows: None,
            cancel: None,
        };
        assert_eq!(link_chat(options()).unwrap().rows_scanned, 2);
        let connection = Connection::open(&manifest).unwrap();
        connection
            .execute(
                "update image_occurrence_scan_state set linker_version=1 where conversation_table='group_1'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(999,'group_1',1)",
                [],
            )
            .unwrap();
        drop(connection);

        let mut partial_options = options();
        partial_options.max_rows = Some(1);
        let partial = link_chat(partial_options).unwrap();
        assert_eq!(partial.rows_scanned, 1);
        let connection = Connection::open(&manifest).unwrap();
        let partial_state: (i64, String) = connection
            .query_row(
                "select last_rowid,scan_mode from image_occurrence_scan_state where conversation_table='group_1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(partial_state, (1, "mmp".to_string()));
        drop(connection);
        let upgraded = link_chat(options()).unwrap();
        assert_eq!(upgraded.rows_scanned, 1);
        let connection = Connection::open(&manifest).unwrap();
        let preserved: i64 = connection
            .query_row(
                "select count(*) from image_asset_occurrences where asset_id=999 and conversation_table='group_1' and message_rowid=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved, 1);
        let version: i64 = connection
            .query_row(
                "select linker_version from image_occurrence_scan_state where conversation_table='group_1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, LINKER_VERSION);
        let _ = std::fs::remove_dir_all(root);
    }

    fn write_chat_fixture(path: &Path, rows: usize, sender: i64) {
        let con = Connection::open(path).unwrap();
        con.execute(
            "create table group_1(Time integer,Rand integer,SenderUin integer,MsgContent blob,Info blob)",
            [],
        )
        .unwrap();
        for index in 0..rows {
            con.execute(
                "insert into group_1 values(?1,?2,?3,?4,?5)",
                params![
                    1_700_000_000i64 + index as i64,
                    index as i64,
                    sender,
                    Vec::<u8>::new(),
                    Vec::<u8>::new()
                ],
            )
            .unwrap();
        }
    }
}

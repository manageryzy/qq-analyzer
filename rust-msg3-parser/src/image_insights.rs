//! Popular-image analytics built from the persisted chat provenance.
//!
//! SSCD same-image membership intentionally lives only in [`SameImageCache`].
//! This module never creates or writes an `image_same_groups` table and never
//! initializes an inference runtime: it only compares vectors already stored
//! in the image manifest.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, UNIX_EPOCH};

use base64::Engine;
use chrono::{Duration as ChronoDuration, Local, NaiveDate, TimeZone};
use rusqlite::config::DbConfig;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::image_index;
use crate::msg3_log_service_tables::{quote_ident, split_table};
use crate::sscd_vector_index::SscdVectorIndex;

pub const SAME_IMAGE_THRESHOLD: f64 = 0.98;
pub const CANDIDATE_LIMIT: usize = 2_000;
const SAME_CACHE_MAX_GROUPS: usize = 4_096;
const SAME_CACHE_MAX_MEMBERS: usize = 100_000;
const SAME_GROUP_PARALLELISM: usize = 4;
const SAME_CANDIDATE_LIMIT: usize = 8_192;
const SAME_ANN_CANDIDATE_LIMIT: usize = 512;
const COMPARE_CANDIDATE_LIMIT: usize = 200;
const RANKING_CACHE_LIMIT: usize = 16;
const OVERVIEW_CACHE_LIMIT: usize = 16;
const ANALYSIS_BATCH_SIZE: usize = 2_000;
const LEGACY_MIGRATION_BATCH_SIZE: usize = 10_000;
const LEGACY_MIGRATION_TABLE: &str = "image_asset_occurrences_v3_migration";
const SUPERSEDED_OCCURRENCE_TABLE: &str = "image_asset_occurrences_v2_legacy";
const POPULARITY_SCHEMA_VERSION: i64 = 4;
const MIN_UPGRADABLE_POPULARITY_SCHEMA_VERSION: i64 = 2;

#[derive(Clone, Debug, Serialize)]
pub struct PopularityAnalysisReport {
    pub account: String,
    pub cancelled: bool,
    pub cube_rebuilt: bool,
    pub occurrences: i64,
    pub facts: i64,
    pub cube_rows: i64,
    pub tables_complete: i64,
    pub tables_total: i64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InsightsQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub senders: Vec<String>,
    pub tables: Vec<String>,
    pub conversation_types: Vec<String>,
    pub rank: String,
    pub window: i64,
}

impl InsightsQuery {
    pub fn from_params(params: &HashMap<String, String>) -> anyhow::Result<Self> {
        let from = normalized_date(params.get("from"))?;
        let to = normalized_date(params.get("to"))?;
        if matches!((&from, &to), (Some(from), Some(to)) if from > to) {
            anyhow::bail!("from must be on or before to");
        }
        let rank = match params.get("rank").map(String::as_str).unwrap_or("popular") {
            "popular" | "reach" | "growth" | "new" | "revival" => params
                .get("rank")
                .cloned()
                .unwrap_or_else(|| "popular".to_string()),
            other => anyhow::bail!("unsupported rank mode {other}"),
        };
        let window = params
            .get("window")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(90)
            .clamp(1, 3_650);
        Ok(Self {
            from,
            to,
            senders: csv_values(params.get("senders")),
            tables: csv_values(params.get("tables")),
            conversation_types: csv_values(params.get("conversation_type")),
            rank,
            window,
        })
    }

    pub fn task_key(&self) -> String {
        serde_json::to_string(self).expect("serializable insight query")
    }

    pub fn overview_key(&self) -> String {
        serde_json::to_string(&(
            &self.from,
            &self.to,
            &self.senders,
            &self.tables,
            &self.conversation_types,
        ))
        .expect("serializable insight overview query")
    }
}

#[derive(Default)]
pub(crate) struct InsightsOverviewCache {
    values: HashMap<String, Value>,
    recency: VecDeque<String>,
}

impl InsightsOverviewCache {
    pub(crate) fn get(&mut self, key: &str) -> Option<Value> {
        let value = self.values.get(key).cloned()?;
        self.recency.retain(|cached| cached != key);
        self.recency.push_back(key.to_string());
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: String, value: Value) {
        self.values.insert(key.clone(), value);
        self.recency.retain(|cached| cached != &key);
        self.recency.push_back(key);
        while self.values.len() > OVERVIEW_CACHE_LIMIT {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            self.values.remove(&oldest);
        }
    }
}

fn normalized_date(value: Option<&String>) -> anyhow::Result<Option<String>> {
    let Some(value) = value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("date must use YYYY-MM-DD"))?;
    Ok(Some(value.to_string()))
}

fn csv_values(value: Option<&String>) -> Vec<String> {
    let mut values = value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .map(|value| {
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value)
                .trim()
        })
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

pub fn build_popularity_analysis(
    root: &Path,
    account: &str,
    chat_db: &Path,
    cancel: Option<&Arc<AtomicBool>>,
) -> anyhow::Result<PopularityAnalysisReport> {
    let started = Instant::now();
    if !chat_db.is_file() {
        anyhow::bail!("prepared chat database is missing: {}", chat_db.display());
    }
    let manifest_path = image_index::default_manifest_path(root, account);
    let mut popularity_schema_ready = false;
    if manifest_path.is_file() {
        let manifest = Connection::open_with_flags(
            &manifest_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        popularity_schema_ready = popularity_state_table_exists(&manifest)?
            && manifest.query_row(
                r#"select exists(
                       select 1 from sqlite_master
                       where type='table' and name='image_popularity_daily_conversation'
                   )"#,
                [],
                |row| row.get(0),
            )?;
        if let Some(report) = completed_analysis_report(&manifest, account, &started)? {
            if !legacy_fact_table_exists(&manifest)? {
                return Ok(report);
            }
        }
    }
    if !popularity_schema_ready {
        image_index::prepare_web_manifest(root, account)?;
    }
    let mut manifest = Connection::open(&manifest_path)?;
    manifest.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    manifest.execute_batch(
        r#"pragma busy_timeout=5000;
           pragma journal_mode=WAL;
           pragma synchronous=NORMAL;
           pragma wal_autocheckpoint=0;"#,
    )?;
    if let Some(cancel) = cancel {
        let cancel = Arc::clone(cancel);
        manifest.progress_handler(10_000, Some(move || cancel.load(AtomicOrdering::Relaxed)));
    }

    if let Err(error) = upgrade_completed_popularity_cache(&mut manifest) {
        if is_cancelled(cancel) {
            return cancelled_analysis_report(&mut manifest, account, &started);
        }
        return Err(error);
    }
    if let Some(report) = completed_analysis_report(&manifest, account, &started)? {
        if cleanup_redundant_legacy_facts(&mut manifest)? {
            return Ok(report);
        }
    }
    let chat = Connection::open_with_flags(
        chat_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    mark_build_not_ready(&manifest)?;
    if let Err(error) = migrate_legacy_facts(&mut manifest, cancel) {
        if is_cancelled(cancel) {
            return cancelled_analysis_report(&mut manifest, account, &started);
        }
        return Err(error);
    }
    if is_cancelled(cancel) {
        return cancelled_analysis_report(&mut manifest, account, &started);
    }
    let tables = match occurrence_tables(&manifest) {
        Ok(tables) => tables,
        Err(_) if is_cancelled(cancel) => {
            return cancelled_analysis_report(&mut manifest, account, &started);
        }
        Err(error) => return Err(error),
    };
    if !analysis_states_are_current(&manifest)? {
        if let Err(error) = materialize_analysis_states(&mut manifest, &tables) {
            if is_cancelled(cancel) {
                return cancelled_analysis_report(&mut manifest, account, &started);
            }
            return Err(error);
        }
    }
    for table in &tables {
        if is_cancelled(cancel) {
            break;
        }
        if let Err(error) = analyze_table(&chat, &mut manifest, table, cancel) {
            if is_cancelled(cancel) {
                return cancelled_analysis_report(&mut manifest, account, &started);
            }
            return Err(error);
        }
    }
    let mut cancelled = is_cancelled(cancel);
    let mut cube_rebuilt = false;
    if !cancelled {
        if let Err(error) = rebuild_cube_and_summaries(&mut manifest) {
            if is_cancelled(cancel) {
                cancelled = true;
            } else {
                return Err(error);
            }
        } else {
            cube_rebuilt = true;
        }
    }
    manifest.progress_handler(0, None::<fn() -> bool>);
    if cancelled {
        mark_build_not_ready(&manifest)?;
    }
    let (occurrences, facts, cube_rows) = if cube_rebuilt {
        cached_analysis_counts(&manifest)?
    } else {
        analysis_counts(&manifest)?
    };
    let (tables_total, tables_complete) = manifest.query_row(
        "select count(*),coalesce(sum(completed),0) from image_popularity_analysis_state",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(PopularityAnalysisReport {
        account: account.to_string(),
        cancelled,
        cube_rebuilt,
        occurrences,
        facts,
        cube_rows,
        tables_complete,
        tables_total,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn cancelled_analysis_report(
    manifest: &mut Connection,
    account: &str,
    started: &Instant,
) -> anyhow::Result<PopularityAnalysisReport> {
    manifest.progress_handler(0, None::<fn() -> bool>);
    mark_build_not_ready(manifest)?;
    // Cancellation must return promptly. Recounting millions of occurrence
    // rows here made the task remain in `stopping` long after SQLite had
    // interrupted the actual work. The per-conversation state and build-state
    // row already contain suitable progress counters and are tiny.
    let (occurrences, facts, cube_rows) = cached_analysis_counts(manifest)?;
    let (tables_total, tables_complete) = manifest.query_row(
        "select count(*),coalesce(sum(completed),0) from image_popularity_analysis_state",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(PopularityAnalysisReport {
        account: account.to_string(),
        cancelled: true,
        cube_rebuilt: false,
        occurrences,
        facts,
        cube_rows,
        tables_complete,
        tables_total,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn completed_analysis_report(
    manifest: &Connection,
    account: &str,
    started: &Instant,
) -> anyhow::Result<Option<PopularityAnalysisReport>> {
    if !popularity_state_table_exists(manifest)? {
        return Ok(None);
    }
    let Some(previous) = popularity_build_state(manifest)?
        .filter(|state| state.schema_version == POPULARITY_SCHEMA_VERSION && state.summary_ready)
    else {
        return Ok(None);
    };
    let (tables_total, tables_complete) = manifest.query_row(
        "select count(*),coalesce(sum(completed),0) from image_popularity_analysis_state",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Some(PopularityAnalysisReport {
        account: account.to_string(),
        cancelled: false,
        cube_rebuilt: false,
        occurrences: previous.occurrence_count,
        facts: previous.fact_count,
        cube_rows: previous.cube_row_count,
        tables_complete,
        tables_total,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }))
}

fn popularity_state_table_exists(manifest: &Connection) -> anyhow::Result<bool> {
    Ok(manifest.query_row(
        "select exists(select 1 from sqlite_master where type='table' and name='image_popularity_build_state')",
        [],
        |row| row.get(0),
    )?)
}

fn legacy_fact_table_exists(manifest: &Connection) -> anyhow::Result<bool> {
    Ok(manifest.query_row(
        "select exists(select 1 from sqlite_master where type='table' and name='image_occurrence_message_facts')",
        [],
        |row| row.get(0),
    )?)
}

/// Removes the superseded fact table once every useful legacy row is already
/// represented by the unified occurrence facts. This is intentionally checked
/// before returning a completed analysis so upgraded databases converge
/// without another message scan.
fn cleanup_redundant_legacy_facts(manifest: &mut Connection) -> anyhow::Result<bool> {
    if !legacy_fact_table_exists(manifest)? {
        return Ok(true);
    }
    let migration_incomplete = manifest
        .query_row(
            "select completed=0 from image_occurrence_fact_migration_state where id=1",
            [],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    if migration_incomplete {
        return Ok(false);
    }
    let missing_useful_facts: bool = manifest.query_row(
        r#"select exists(
               select 1
               from image_occurrence_message_facts legacy
               join image_asset_occurrences occurrence
                 on occurrence.asset_id=legacy.asset_id
                and occurrence.conversation_table=legacy.conversation_table
                and occurrence.message_rowid=legacy.message_rowid
               where occurrence.facts_updated_at=''
               limit 1
           )"#,
        [],
        |row| row.get(0),
    )?;
    if missing_useful_facts {
        return Ok(false);
    }
    let tx = manifest.transaction()?;
    tx.execute("drop table image_occurrence_message_facts", [])?;
    tx.execute("delete from image_occurrence_fact_migration_state", [])?;
    tx.commit()?;
    Ok(true)
}

fn occurrence_tables(con: &Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = con.prepare(
        "select distinct conversation_table from image_asset_occurrences order by conversation_table",
    )?;
    let tables = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(tables)
}

fn migrate_legacy_facts(
    manifest: &mut Connection,
    cancel: Option<&Arc<AtomicBool>>,
) -> anyhow::Result<()> {
    let exists: bool = manifest.query_row(
        "select exists(select 1 from sqlite_master where type='table' and name='image_occurrence_message_facts')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let has_scan_mode: bool = manifest.query_row(
        "select exists(select 1 from pragma_table_info('image_occurrence_fact_migration_state') where name='scan_mode')",
        [],
        |row| row.get(0),
    )?;
    if !has_scan_mode {
        manifest.execute(
            "alter table image_occurrence_fact_migration_state add column scan_mode text not null default 'occurrence_rowid_v1'",
            [],
        )?;
    }
    if try_adopt_legacy_fact_table(manifest, cancel)? {
        return Ok(());
    }
    let max_rowid: i64 = manifest.query_row(
        "select coalesce(max(rowid),0) from image_occurrence_message_facts",
        [],
        |row| row.get(0),
    )?;
    let (saved_rowid, scan_mode, saved_processed): (i64, String, i64) = manifest
        .query_row(
            r#"select last_occurrence_rowid,scan_mode,processed_occurrences
               from image_occurrence_fact_migration_state where id=1"#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .unwrap_or((0, String::new(), 0));
    let staging_exists: bool = manifest.query_row(
        "select exists(select 1 from sqlite_master where type='table' and name=?1)",
        [LEGACY_MIGRATION_TABLE],
        |row| row.get(0),
    )?;
    let mut last_rowid = if scan_mode == "sequential_occurrence_rebuild_v4" && staging_exists {
        saved_rowid.min(max_rowid)
    } else {
        0
    };
    let mut processed = if scan_mode == "sequential_occurrence_rebuild_v4" && staging_exists {
        saved_processed
    } else if matches!(
        scan_mode.as_str(),
        "legacy_fact_rowid_v2" | "target_occurrence_rowid_v3"
    ) {
        saved_rowid
    } else {
        0
    }
    .clamp(0, max_rowid);
    if last_rowid == 0 {
        let migrated_at = current_timestamp();
        let tx = manifest.transaction()?;
        tx.execute(
            &format!("drop table if exists {LEGACY_MIGRATION_TABLE}"),
            [],
        )?;
        tx.execute_batch(&format!(
            r#"create table {LEGACY_MIGRATION_TABLE} (
                   asset_id integer not null,
                   conversation_table text not null,
                   message_rowid integer not null,
                   linked_at text not null default '',
                   message_time integer,
                   message_date text not null default '',
                   sender_uin text not null default '',
                   conversation_type text not null default '',
                   facts_updated_at text not null default ''
               );"#
        ))?;
        tx.execute(
            r#"insert into image_occurrence_fact_migration_state(
                   id,last_occurrence_rowid,max_occurrence_rowid,completed,
                   updated_at,scan_mode,processed_occurrences,total_occurrences
               ) values(
                   1,0,?1,0,?2,'sequential_occurrence_rebuild_v4',?3,?1
               )
               on conflict(id) do update set
                   last_occurrence_rowid=0,
                   max_occurrence_rowid=excluded.max_occurrence_rowid,
                   completed=0,
                   updated_at=excluded.updated_at,
                   scan_mode=excluded.scan_mode,
                   processed_occurrences=excluded.processed_occurrences,
                   total_occurrences=excluded.total_occurrences"#,
            params![max_rowid, migrated_at, processed],
        )?;
        tx.commit()?;
    }
    while last_rowid < max_rowid && !is_cancelled(cancel) {
        let batch_end = (last_rowid + LEGACY_MIGRATION_BATCH_SIZE as i64).min(max_rowid);
        let migrated_at = current_timestamp();
        let tx = manifest.transaction()?;
        let migrated = match tx.execute(
            &format!(
                r#"insert into {LEGACY_MIGRATION_TABLE}(
                       asset_id,conversation_table,message_rowid,linked_at,
                       message_time,message_date,sender_uin,conversation_type,
                       facts_updated_at
                   )
                   select asset_id,conversation_table,message_rowid,
                          coalesce(nullif(updated_at,''),?3),
                          message_time,message_date,sender_uin,conversation_type,
                          coalesce(nullif(updated_at,''),?3)
                   from image_occurrence_message_facts
                   where rowid>?1 and rowid<=?2
                   order by rowid"#
            ),
            params![last_rowid, batch_end, migrated_at],
        ) {
            Ok(migrated) => migrated,
            Err(_) if is_cancelled(cancel) => {
                drop(tx);
                break;
            }
            Err(error) => return Err(error.into()),
        };
        if is_cancelled(cancel) {
            drop(tx);
            break;
        }
        last_rowid = batch_end;
        if migrated > 0 {
            processed = processed.max(last_rowid).min(max_rowid);
        }
        tx.execute(
            r#"insert into image_occurrence_fact_migration_state(
                   id,last_occurrence_rowid,max_occurrence_rowid,completed,
                   updated_at,scan_mode,processed_occurrences,total_occurrences
               ) values(
                   1,?1,?2,?3,?4,'sequential_occurrence_rebuild_v4',?5,?2
               )
               on conflict(id) do update set
                   last_occurrence_rowid=excluded.last_occurrence_rowid,
                   max_occurrence_rowid=excluded.max_occurrence_rowid,
                   completed=excluded.completed,
                   updated_at=excluded.updated_at,
                   scan_mode=excluded.scan_mode,
                   processed_occurrences=excluded.processed_occurrences,
                   total_occurrences=excluded.total_occurrences"#,
            params![
                last_rowid,
                max_rowid,
                last_rowid >= max_rowid,
                migrated_at,
                processed
            ],
        )?;
        tx.commit()?;
    }
    let mut finalized = false;
    if last_rowid >= max_rowid && !is_cancelled(cancel) {
        finalized = finalize_legacy_fact_migration(manifest, cancel)?;
    }
    if last_rowid >= max_rowid && !finalized && !is_cancelled(cancel) {
        anyhow::bail!("legacy fact migration finalization did not complete");
    }
    Ok(())
}

/// The completed legacy table already has the unified table's primary key and
/// all message facts. When no new occurrence row has appeared since it was
/// built, adopt it in place instead of copying millions of rows through the
/// Windows-mounted manifest. Its existing conversation/date and primary-key
/// indexes cover the production query shapes, so the schema swap is atomic and
/// does not hide another multi-million-row index build at 100% progress.
fn try_adopt_legacy_fact_table(
    manifest: &mut Connection,
    cancel: Option<&Arc<AtomicBool>>,
) -> anyhow::Result<bool> {
    let rowid_bounds = || -> rusqlite::Result<(i64, i64)> {
        manifest.query_row(
            r#"select
                   (select coalesce(max(rowid),0)
                    from image_occurrence_message_facts),
                   (select coalesce(max(rowid),0)
                    from image_asset_occurrences)"#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    };
    let (legacy_last_rowid, occurrence_last_rowid) = rowid_bounds()?;
    if legacy_last_rowid != occurrence_last_rowid {
        return Ok(false);
    }
    if is_cancelled(cancel) {
        return Ok(false);
    }
    let (legacy_last_rowid_after, occurrence_last_rowid_after) = rowid_bounds()?;
    if legacy_last_rowid_after != occurrence_last_rowid_after {
        return Ok(false);
    }
    let tx = manifest.transaction()?;
    tx.execute(
        &format!("alter table image_asset_occurrences rename to {SUPERSEDED_OCCURRENCE_TABLE}"),
        [],
    )?;
    tx.execute(
        r#"alter table image_occurrence_message_facts
           rename column updated_at to facts_updated_at"#,
        [],
    )?;
    tx.execute(
        r#"alter table image_occurrence_message_facts
           add column linked_at text not null default ''"#,
        [],
    )?;
    tx.execute(
        r#"alter table image_occurrence_message_facts
           rename to image_asset_occurrences"#,
        [],
    )?;
    tx.execute("delete from image_occurrence_fact_migration_state", [])?;
    tx.commit()?;
    Ok(true)
}

/// Reclaims tables left behind by the metadata-only legacy swap. This is
/// intentionally invoked only after the user-visible analysis task has left
/// the running state: dropping a multi-million-row SQLite B-tree on a
/// Windows-mounted disk can take minutes even though readers remain available
/// through WAL snapshots.
pub fn cleanup_superseded_occurrence_tables(root: &Path, account: &str) -> anyhow::Result<()> {
    let manifest_path = image_index::default_manifest_path(root, account);
    if !manifest_path.is_file() {
        return Ok(());
    }
    let manifest = Connection::open(&manifest_path)?;
    manifest.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    let legacy_facts_remain: bool = manifest.query_row(
        r#"select exists(
               select 1 from sqlite_master
               where type='table' and name='image_occurrence_message_facts'
           )"#,
        [],
        |row| row.get(0),
    )?;
    if legacy_facts_remain {
        return Ok(());
    }
    let superseded_tables_remain: bool = manifest.query_row(
        r#"select exists(
               select 1 from sqlite_master
               where type='table'
                 and name in (
                     'image_asset_occurrences_v2_legacy',
                     'image_asset_occurrences_v3_migration'
                 )
           )"#,
        [],
        |row| row.get(0),
    )?;
    if !superseded_tables_remain {
        return Ok(());
    }
    manifest.execute_batch(
        r#"pragma busy_timeout=5000;
           pragma journal_mode=WAL;
           pragma synchronous=NORMAL;
           pragma wal_autocheckpoint=0;
           drop table if exists image_asset_occurrences_v2_legacy;
           drop table if exists image_asset_occurrences_v3_migration;"#,
    )?;
    Ok(())
}

fn finalize_legacy_fact_migration(
    manifest: &mut Connection,
    cancel: Option<&Arc<AtomicBool>>,
) -> anyhow::Result<bool> {
    let (legacy_last_rowid, occurrence_last_rowid): (i64, i64) = manifest.query_row(
        r#"select
               (select coalesce(max(rowid),0)
                from image_occurrence_message_facts),
               (select coalesce(max(rowid),0)
                from image_asset_occurrences)"#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if let Err(error) = manifest.execute(
        &format!(
            r#"create unique index if not exists
                   image_asset_occurrences_v3_migration_pk
               on {LEGACY_MIGRATION_TABLE}(
                   asset_id,conversation_table,message_rowid
               )"#
        ),
        [],
    ) {
        if is_cancelled(cancel) {
            return Ok(false);
        }
        return Err(error.into());
    }
    if is_cancelled(cancel) {
        return Ok(false);
    }
    if occurrence_last_rowid != legacy_last_rowid {
        if let Err(error) = manifest.execute(
            &format!(
                r#"insert or ignore into {LEGACY_MIGRATION_TABLE}(
                       asset_id,conversation_table,message_rowid,linked_at,
                       message_time,message_date,sender_uin,conversation_type,
                       facts_updated_at
                   )
                   select asset_id,conversation_table,message_rowid,linked_at,
                          message_time,message_date,sender_uin,conversation_type,
                          facts_updated_at
                   from image_asset_occurrences"#
            ),
            [],
        ) {
            if is_cancelled(cancel) {
                return Ok(false);
            }
            return Err(error.into());
        }
    }
    if is_cancelled(cancel) {
        return Ok(false);
    }
    if let Err(error) = manifest.execute(
        &format!(
            r#"create index if not exists
                   image_asset_occurrences_v3_migration_conversation_idx
               on {LEGACY_MIGRATION_TABLE}(
                   conversation_table,message_rowid
               )"#
        ),
        [],
    ) {
        if is_cancelled(cancel) {
            return Ok(false);
        }
        return Err(error.into());
    }
    if is_cancelled(cancel) {
        return Ok(false);
    }
    let tx = manifest.transaction()?;
    tx.execute("drop table image_asset_occurrences", [])?;
    tx.execute(
        &format!("alter table {LEGACY_MIGRATION_TABLE} rename to image_asset_occurrences"),
        [],
    )?;
    image_index::mark_occurrence_content_changed(&tx)?;
    tx.execute("drop table image_occurrence_message_facts", [])?;
    tx.execute("delete from image_occurrence_fact_migration_state", [])?;
    tx.commit()?;
    Ok(true)
}

fn materialize_analysis_states(manifest: &mut Connection, tables: &[String]) -> anyhow::Result<()> {
    let tx = manifest.transaction()?;
    tx.execute("delete from image_popularity_analysis_state", [])?;
    tx.execute(
        r#"insert into image_popularity_analysis_state(
               conversation_table,last_message_rowid,max_message_rowid,
               occurrence_count,fact_count,completed,updated_at
           )
           select conversation_table,
                  coalesce(sum(case when facts_updated_at<>'' then 1 else 0 end),0),
                  count(*),
                  count(*),
                  coalesce(sum(case when message_time is not null then 1 else 0 end),0),
                  coalesce(sum(case when facts_updated_at<>'' then 1 else 0 end),0)
                      >= count(*),
                  ?1
           from image_asset_occurrences
           group by conversation_table"#,
        [current_timestamp()],
    )?;
    tx.commit()?;
    debug_assert_eq!(
        tables.len() as i64,
        manifest.query_row(
            "select count(*) from image_popularity_analysis_state",
            [],
            |row| row.get::<_, i64>(0)
        )?
    );
    Ok(())
}

fn analysis_states_are_current(manifest: &Connection) -> anyhow::Result<bool> {
    let Some(build) = popularity_build_state(manifest)? else {
        return Ok(false);
    };
    if build.occurrence_count <= 0 {
        return Ok(false);
    }
    let (tables, completed, occurrences, facts): (i64, i64, i64, i64) = manifest.query_row(
        r#"select count(*),coalesce(sum(completed),0),
                      coalesce(sum(occurrence_count),0),
                      coalesce(sum(fact_count),0)
               from image_popularity_analysis_state"#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    Ok(tables > 0
        && completed == tables
        && occurrences == build.occurrence_count
        && facts == build.fact_count)
}

fn analyze_table(
    chat: &Connection,
    manifest: &mut Connection,
    table: &str,
    cancel: Option<&Arc<AtomicBool>>,
) -> anyhow::Result<()> {
    let quoted = quote_ident(table);
    let table_exists: bool = chat.query_row(
        "select exists(select 1 from sqlite_master where type='table' and name=?1)",
        [table],
        |row| row.get(0),
    )?;
    if !table_exists {
        anyhow::bail!("conversation table {table} is missing from prepared chat database");
    }
    while !is_cancelled(cancel) {
        let rowids = {
            let mut stmt = manifest.prepare(
                r#"select distinct message_rowid
                   from image_asset_occurrences
                   where conversation_table=?1 and facts_updated_at=''
                   order by message_rowid
                   limit ?2"#,
            )?;
            let rowids = stmt
                .query_map(params![table, ANALYSIS_BATCH_SIZE as i64], |row| {
                    row.get::<_, i64>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rowids
        };
        if rowids.is_empty() {
            break;
        }
        let mut message_stmt = chat.prepare(&format!(
            "select Time,SenderUin from {quoted} where rowid=?1"
        ))?;
        let mut batch = Vec::<(i64, Option<(i64, String, String)>)>::new();
        for rowid in rowids {
            let message = message_stmt
                .query_row([rowid], |row| {
                    Ok((integer_value(row.get_ref(0)?)?, text_value(row.get_ref(1)?)))
                })
                .optional()?;
            batch.push((
                rowid,
                message.map(|(time, sender)| (time, message_date(time), sender)),
            ));
        }
        drop(message_stmt);
        let tx = manifest.transaction()?;
        let conversation_type = split_table(table).0;
        let updated_at = current_timestamp();
        for (rowid, message) in batch {
            if let Some((time, date, sender)) = message {
                tx.execute(
                    r#"update image_asset_occurrences
                       set message_time=?3,message_date=?4,sender_uin=?5,
                           conversation_type=?6,facts_updated_at=?7
                       where conversation_table=?1 and message_rowid=?2"#,
                    params![
                        table,
                        rowid,
                        time,
                        date,
                        sender,
                        conversation_type,
                        updated_at
                    ],
                )?;
            } else {
                tx.execute(
                    r#"update image_asset_occurrences
                       set facts_updated_at=?3
                       where conversation_table=?1 and message_rowid=?2"#,
                    params![table, rowid, updated_at],
                )?;
            }
        }
        let (occurrences, inspected, fact_count): (i64, i64, i64) = tx.query_row(
            r#"select count(*),
                      coalesce(sum(case when facts_updated_at<>'' then 1 else 0 end),0),
                      coalesce(sum(case when message_time is not null then 1 else 0 end),0)
               from image_asset_occurrences where conversation_table=?1"#,
            [table],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        tx.execute(
            r#"update image_popularity_analysis_state
               set last_message_rowid=?2,max_message_rowid=?3,
                   occurrence_count=?3,fact_count=?4,completed=?5,updated_at=?6
               where conversation_table=?1"#,
            params![
                table,
                inspected,
                occurrences,
                fact_count,
                inspected >= occurrences,
                updated_at
            ],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct PopularityBuildState {
    schema_version: i64,
    occurrence_count: i64,
    fact_count: i64,
    cube_row_count: i64,
    summary_ready: bool,
    refreshed_at: String,
}

fn popularity_build_state(con: &Connection) -> anyhow::Result<Option<PopularityBuildState>> {
    con.query_row(
        r#"select schema_version,occurrence_count,fact_count,cube_row_count,
                  summary_ready,refreshed_at
           from image_popularity_build_state where id=1"#,
        [],
        |row| {
            Ok(PopularityBuildState {
                schema_version: row.get(0)?,
                occurrence_count: row.get(1)?,
                fact_count: row.get(2)?,
                cube_row_count: row.get(3)?,
                summary_ready: row.get(4)?,
                refreshed_at: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn upgrade_completed_popularity_cache(manifest: &mut Connection) -> anyhow::Result<bool> {
    let Some(previous) = popularity_build_state(manifest)? else {
        return Ok(false);
    };
    if previous.schema_version < MIN_UPGRADABLE_POPULARITY_SCHEMA_VERSION
        || previous.schema_version >= POPULARITY_SCHEMA_VERSION
        || !previous.summary_ready
    {
        return Ok(false);
    }
    normalize_occurrence_message_dates(manifest)?;
    rebuild_cube_and_summaries(manifest)?;
    Ok(true)
}

fn normalize_occurrence_message_dates(manifest: &mut Connection) -> anyhow::Result<()> {
    let tx = manifest.transaction()?;
    tx.execute(
        r#"update image_asset_occurrences
           set message_date=coalesce(
               strftime(
                   '%Y-%m-%d',
                   case
                       when message_time>10000000000 or message_time<(-10000000000)
                           then message_time/1000
                       else message_time
                   end,
                   'unixepoch','localtime'
               ),
               '1970-01-01'
           )
           where message_time is not null
             and message_date<>coalesce(
                 strftime(
                     '%Y-%m-%d',
                     case
                         when message_time>10000000000 or message_time<(-10000000000)
                             then message_time/1000
                         else message_time
                     end,
                     'unixepoch','localtime'
                 ),
                 '1970-01-01'
             )"#,
        [],
    )?;
    tx.commit()?;
    Ok(())
}

fn rebuild_cube_and_summaries(manifest: &mut Connection) -> anyhow::Result<()> {
    let refreshed_at = current_timestamp();
    // `image_exact_groups` lives in the 60+ GiB archive manifest. Looking up
    // one representative for every occurrence turns an otherwise sequential
    // scan into millions of random reads on a Windows-mounted drive. Copy the
    // compact mapping once, sequentially, into SQLite's local temp storage.
    // FILE keeps memory bounded; the temp B-tree is removed with this
    // connection and is never persisted as analysis state.
    manifest.execute_batch(
        r#"
        pragma temp_store=FILE;
        drop table if exists temp.image_exact_representative_lookup;
        create temp table image_exact_representative_lookup(
            asset_id integer primary key,
            representative_asset_id integer not null,
            member_count integer not null
        ) without rowid;
        insert into image_exact_representative_lookup(
            asset_id,representative_asset_id,member_count
        )
        select asset_id,representative_asset_id,member_count
        from image_exact_groups
        order by asset_id;
        "#,
    )?;
    let tx = manifest.transaction()?;
    // Maintaining every secondary B-tree row-by-row makes the multi-million
    // row rebuild pathologically slow on the Windows-mounted archive. Readers
    // keep seeing the old schema through WAL until commit, while this writer
    // bulk-loads the primary table and then builds the four useful indexes.
    tx.execute_batch(
        r#"
        drop index if exists image_popularity_cube_rank_idx;
        drop index if exists image_popularity_cube_date_idx;
        drop index if exists image_popularity_cube_sender_idx;
        drop index if exists image_popularity_cube_conversation_idx;
        drop index if exists image_popularity_cube_sender_cover_idx;
        drop index if exists image_popularity_cube_conversation_cover_idx;
        drop index if exists image_popularity_cube_date_cover_idx;
        "#,
    )?;
    tx.execute("delete from image_popularity_cube", [])?;
    tx.execute_batch(
        r#"
        insert into image_popularity_cube(
            representative_asset_id,message_date,sender_uin,
            conversation_table,conversation_type,reference_count
        )
        select representative_asset_id,message_date,sender_uin,
               conversation_table,conversation_type,count(*)
        from image_asset_occurrences occurrence not indexed
        join temp.image_exact_representative_lookup exact_group
          on exact_group.asset_id=occurrence.asset_id
        where occurrence.message_time is not null
          and occurrence.message_date<>''
        group by representative_asset_id,message_date,sender_uin,
                 conversation_table,conversation_type;

        -- The common path above needs one full GROUP BY instead of first
        -- sorting all occurrence rows for DISTINCT and then sorting them
        -- again for the cube. Only exact groups with multiple assets can
        -- over-count a message, so compute and subtract that much smaller
        -- correction set separately.
        insert into image_popularity_cube(
            representative_asset_id,message_date,sender_uin,
            conversation_table,conversation_type,reference_count
        )
        select representative_asset_id,message_date,sender_uin,
               conversation_table,conversation_type,-sum(duplicate_count)
        from (
            select exact_group.representative_asset_id,
                   occurrence.message_date,occurrence.sender_uin,
                   occurrence.conversation_table,occurrence.conversation_type,
                   occurrence.message_rowid,count(*)-1 as duplicate_count
            from image_asset_occurrences occurrence not indexed
            join temp.image_exact_representative_lookup exact_group
              on exact_group.asset_id=occurrence.asset_id
            where occurrence.message_time is not null
              and occurrence.message_date<>''
              and exact_group.member_count>1
            group by exact_group.representative_asset_id,
                     occurrence.message_date,occurrence.sender_uin,
                     occurrence.conversation_table,occurrence.conversation_type,
                     occurrence.message_rowid
            having count(*)>1
        )
        group by representative_asset_id,message_date,sender_uin,
                 conversation_table,conversation_type
        on conflict(
            representative_asset_id,message_date,sender_uin,
            conversation_table,conversation_type
        ) do update set reference_count=
            image_popularity_cube.reference_count+excluded.reference_count;

        create index image_popularity_cube_rank_idx
            on image_popularity_cube(reference_count desc, representative_asset_id);
        create index image_popularity_cube_sender_cover_idx
            on image_popularity_cube(
                sender_uin, message_date, conversation_table, conversation_type,
                representative_asset_id, reference_count
            );
        create index image_popularity_cube_conversation_cover_idx
            on image_popularity_cube(
                conversation_table, message_date, sender_uin, conversation_type,
                representative_asset_id, reference_count
            );
        create index image_popularity_cube_date_cover_idx
            on image_popularity_cube(
                message_date, sender_uin, conversation_table, conversation_type,
                representative_asset_id, reference_count
            );

        delete from image_popularity_summary_cache;
        "#,
    )?;
    rebuild_daily_conversation_cache(&tx)?;
    insert_summary_scope(&tx, "all", "''", "1=1", &refreshed_at)?;
    insert_summary_scope(
        &tx,
        "date",
        "message_date",
        "1=1 group by message_date",
        &refreshed_at,
    )?;
    insert_summary_scope(
        &tx,
        "sender",
        "sender_uin",
        "1=1 group by sender_uin",
        &refreshed_at,
    )?;
    insert_summary_scope(
        &tx,
        "conversation",
        "conversation_table",
        "1=1 group by conversation_table",
        &refreshed_at,
    )?;
    let occurrences: i64 =
        tx.query_row("select count(*) from image_asset_occurrences", [], |row| {
            row.get(0)
        })?;
    let facts: i64 = tx.query_row(
        "select count(*) from image_asset_occurrences where message_time is not null",
        [],
        |row| row.get(0),
    )?;
    let cube_rows: i64 = tx.query_row("select count(*) from image_popularity_cube", [], |row| {
        row.get(0)
    })?;
    tx.execute(
        r#"insert into image_popularity_build_state(
               id,schema_version,occurrence_count,fact_count,cube_row_count,
           summary_ready,refreshed_at
           ) values(1,?1,?2,?3,?4,1,?5)
           on conflict(id) do update set
               schema_version=excluded.schema_version,
               occurrence_count=excluded.occurrence_count,
               fact_count=excluded.fact_count,
               cube_row_count=excluded.cube_row_count,
               summary_ready=1,
               refreshed_at=excluded.refreshed_at"#,
        params![
            POPULARITY_SCHEMA_VERSION,
            occurrences,
            facts,
            cube_rows,
            refreshed_at
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn rebuild_daily_conversation_cache(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    tx.execute_batch(
        r#"
        delete from image_popularity_daily_conversation;
        insert into image_popularity_daily_conversation(
            message_date,conversation_table,conversation_type,reference_count
        )
        select message_date,conversation_table,conversation_type,sum(reference_count)
        from image_popularity_cube
        group by message_date,conversation_table,conversation_type;
        "#,
    )?;
    Ok(())
}

fn insert_summary_scope(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    key_sql: &str,
    tail_sql: &str,
    refreshed_at: &str,
) -> anyhow::Result<()> {
    let sql = format!(
        r#"insert into image_popularity_summary_cache(
               scope,scope_key,reference_count,image_count,sender_count,
               conversation_count,first_date,last_date,refreshed_at
           )
           select ?1,{key_sql},coalesce(sum(reference_count),0),
                  count(distinct representative_asset_id),
                  count(distinct sender_uin),count(distinct conversation_table),
                  coalesce(min(message_date),''),coalesce(max(message_date),''),?2
           from image_popularity_cube where {tail_sql}"#
    );
    tx.execute(&sql, params![scope, refreshed_at])?;
    Ok(())
}

fn mark_build_not_ready(manifest: &Connection) -> anyhow::Result<()> {
    manifest.execute(
        r#"insert into image_popularity_build_state(
               id,schema_version,occurrence_count,fact_count,cube_row_count,
               summary_ready,refreshed_at
           ) values(1,?1,0,0,0,0,?2)
           on conflict(id) do update set
               schema_version=excluded.schema_version,
               summary_ready=0,
               refreshed_at=excluded.refreshed_at"#,
        params![POPULARITY_SCHEMA_VERSION, current_timestamp()],
    )?;
    Ok(())
}

fn analysis_counts(con: &Connection) -> anyhow::Result<(i64, i64, i64)> {
    Ok((
        con.query_row("select count(*) from image_asset_occurrences", [], |row| {
            row.get(0)
        })?,
        con.query_row(
            "select count(*) from image_asset_occurrences where message_time is not null",
            [],
            |row| row.get(0),
        )?,
        con.query_row("select count(*) from image_popularity_cube", [], |row| {
            row.get(0)
        })?,
    ))
}

fn cached_analysis_counts(con: &Connection) -> anyhow::Result<(i64, i64, i64)> {
    let (occurrences, facts) = con.query_row(
        r#"select coalesce(sum(occurrence_count),0),
                  coalesce(sum(fact_count),0)
           from image_popularity_analysis_state"#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let cube_rows = popularity_build_state(con)?
        .map(|state| state.cube_row_count)
        .unwrap_or(0);
    Ok((occurrences, facts, cube_rows))
}

fn integer_value(value: ValueRef<'_>) -> rusqlite::Result<i64> {
    Ok(match value {
        ValueRef::Integer(value) => value,
        ValueRef::Real(value) => value as i64,
        ValueRef::Text(value) => String::from_utf8_lossy(value).parse().unwrap_or(0),
        _ => 0,
    })
}

fn text_value(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Integer(value) => value.to_string(),
        ValueRef::Real(value) => value.to_string(),
        ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned(),
        ValueRef::Blob(value) => String::from_utf8_lossy(value).into_owned(),
        ValueRef::Null => String::new(),
    }
}

pub(crate) fn message_date(timestamp: i64) -> String {
    message_date_in_timezone(timestamp, &Local)
}

fn message_date_in_timezone<Tz>(timestamp: i64, timezone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let seconds = if timestamp.abs() > 10_000_000_000 {
        timestamp / 1_000
    } else {
        timestamp
    };
    timezone
        .timestamp_opt(seconds, 0)
        .single()
        .map(|value| value.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

fn is_cancelled(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel
        .map(|cancel| cancel.load(AtomicOrdering::Relaxed))
        .unwrap_or(false)
}

fn current_timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Clone, Debug, Serialize)]
pub struct SameImageGroup {
    pub representative_id: i64,
    pub exact_representative_ids: Vec<i64>,
    pub asset_ids: Vec<i64>,
    pub scores: BTreeMap<i64, f64>,
}

impl SameImageGroup {
    fn key(&self) -> i64 {
        self.exact_representative_ids
            .iter()
            .copied()
            .min()
            .unwrap_or(self.representative_id)
    }

    pub fn canonical_key(&self) -> i64 {
        self.key()
    }
}

#[derive(Default)]
struct SameCacheInner {
    identity: String,
    identity_checked_at: Option<Instant>,
    entries: HashMap<i64, Arc<SameImageGroup>>,
    aliases: HashMap<i64, i64>,
    lru: VecDeque<i64>,
    member_count: usize,
    inflight: HashMap<i64, Arc<Inflight>>,
}

#[derive(Default)]
struct Inflight {
    result: Mutex<Option<Result<Arc<SameImageGroup>, String>>>,
    ready: Condvar,
}

#[derive(Default)]
struct ComputeLimiter {
    active: Mutex<usize>,
    available: Condvar,
}

impl ComputeLimiter {
    fn acquire(&self) -> ComputePermit<'_> {
        let mut active = self.active.lock().expect("same-image limiter poisoned");
        while *active >= SAME_GROUP_PARALLELISM {
            active = self
                .available
                .wait(active)
                .expect("same-image limiter poisoned");
        }
        *active += 1;
        ComputePermit { limiter: self }
    }
}

struct ComputePermit<'a> {
    limiter: &'a ComputeLimiter,
}

impl Drop for ComputePermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .limiter
            .active
            .lock()
            .expect("same-image limiter poisoned");
        *active = active.saturating_sub(1);
        self.limiter.available.notify_one();
    }
}

#[derive(Default)]
pub struct SameImageCache {
    inner: Mutex<SameCacheInner>,
    limiter: ComputeLimiter,
    vector_index: SscdVectorIndex,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    invalidations: AtomicU64,
    compute_count: AtomicU64,
    compute_total_us: AtomicU64,
    compute_last_us: AtomicU64,
    compute_max_us: AtomicU64,
}

impl SameImageCache {
    pub fn vector_index_ready(&self) -> bool {
        #[cfg(test)]
        {
            // Unit fixtures intentionally exercise the deterministic SQLite
            // fallback without constructing a persisted multi-million-point
            // service index.
            true
        }
        #[cfg(not(test))]
        {
            self.vector_index.is_ready()
        }
    }

    fn vector_index_available(&self) -> bool {
        #[cfg(test)]
        {
            true
        }
        #[cfg(not(test))]
        {
            cfg!(feature = "image-index-qdrant")
        }
    }

    fn vector_index_error(&self) -> Option<String> {
        self.vector_index.terminal_error()
    }

    /// Loads a compatible persisted HNSW index, or builds it from the SSCD
    /// vectors already present in the read-only image manifest.
    pub fn prepare_vector_index(&self, root: &Path, account: &str) -> anyhow::Result<()> {
        let manifest = image_index::default_manifest_path(root, account);
        let identity = manifest_identity(&manifest, account)?;
        self.vector_index.ensure_ready(&manifest, &identity)
    }

    pub fn result_cache_identity(&self, root: &Path, account: &str) -> anyhow::Result<String> {
        let manifest = image_index::default_manifest_path(root, account);
        Ok(format!(
            "{}:{}:ann-generation-{}",
            overview_cache_identity(root, account)?,
            manifest_identity(&manifest, account)?,
            self.vector_index.generation()
        ))
    }

    /// Return an already-computed group without starting SSCD work.
    ///
    /// Latency-sensitive callers such as the gallery may use this to share
    /// groups warmed by ranking/detail requests without turning a cold page
    /// load into dozens of synchronous manifest scans.
    pub fn peek(&self, asset_id: i64) -> Option<Arc<SameImageGroup>> {
        let mut cache = self.inner.lock().expect("same-image cache poisoned");
        let key = cache.aliases.get(&asset_id).copied()?;
        let group = cache.entries.get(&key).cloned()?;
        touch_lru(&mut cache.lru, key);
        self.hits.fetch_add(1, AtomicOrdering::Relaxed);
        Some(group)
    }

    pub fn resolve(
        &self,
        root: &Path,
        account: &str,
        representative_id: i64,
    ) -> anyhow::Result<Arc<SameImageGroup>> {
        // While the persistent HNSW is being built, do not start the old
        // multi-million-row SQLite candidate path in parallel. Returning an
        // uncached exact-only placeholder keeps detail/compare endpoints
        // responsive; the generation change invalidates real cached groups
        // once the index is ready.
        #[cfg(not(test))]
        if !self.vector_index.is_ready() {
            return Ok(Arc::new(SameImageGroup {
                representative_id,
                exact_representative_ids: vec![representative_id],
                asset_ids: vec![representative_id],
                scores: BTreeMap::from([(representative_id, 1.0)]),
            }));
        }
        let manifest = image_index::default_manifest_path(root, account);
        let should_check_identity = self
            .inner
            .lock()
            .expect("same-image cache poisoned")
            .identity_checked_at
            .is_none_or(|checked| checked.elapsed().as_secs() >= 2);
        let identity = if should_check_identity {
            Some(format!(
                "{}:ann-generation-{}",
                manifest_identity(&manifest, account)?,
                self.vector_index.generation()
            ))
        } else {
            None
        };
        let (inflight, owner) = {
            let mut cache = self.inner.lock().expect("same-image cache poisoned");
            if identity
                .as_ref()
                .is_some_and(|identity| cache.identity != *identity)
            {
                if !cache.identity.is_empty() {
                    self.invalidations.fetch_add(1, AtomicOrdering::Relaxed);
                }
                *cache = SameCacheInner {
                    identity: identity.clone().unwrap_or_default(),
                    identity_checked_at: Some(Instant::now()),
                    ..SameCacheInner::default()
                };
            } else if identity.is_some() {
                cache.identity_checked_at = Some(Instant::now());
            }
            if let Some(key) = cache.aliases.get(&representative_id).copied() {
                if let Some(group) = cache.entries.get(&key).cloned() {
                    touch_lru(&mut cache.lru, key);
                    self.hits.fetch_add(1, AtomicOrdering::Relaxed);
                    return Ok(group);
                }
            }
            if let Some(inflight) = cache.inflight.get(&representative_id).cloned() {
                self.hits.fetch_add(1, AtomicOrdering::Relaxed);
                (inflight, false)
            } else {
                let inflight = Arc::new(Inflight::default());
                cache.inflight.insert(representative_id, inflight.clone());
                self.misses.fetch_add(1, AtomicOrdering::Relaxed);
                (inflight, true)
            }
        };
        if !owner {
            let mut result = inflight
                .result
                .lock()
                .expect("same-image inflight poisoned");
            while result.is_none() {
                result = inflight
                    .ready
                    .wait(result)
                    .expect("same-image inflight poisoned");
            }
            return result
                .as_ref()
                .expect("same-image result present")
                .clone()
                .map_err(anyhow::Error::msg);
        }

        let _permit = self.limiter.acquire();
        let compute_started = Instant::now();
        let computed = compute_same_image_group(&manifest, representative_id, &self.vector_index)
            .map(Arc::new)
            .map_err(|error| error.to_string());
        let compute_us = compute_started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        self.compute_count.fetch_add(1, AtomicOrdering::Relaxed);
        self.compute_total_us
            .fetch_add(compute_us, AtomicOrdering::Relaxed);
        self.compute_last_us
            .store(compute_us, AtomicOrdering::Relaxed);
        self.compute_max_us
            .fetch_max(compute_us, AtomicOrdering::Relaxed);
        let published = {
            let mut cache = self.inner.lock().expect("same-image cache poisoned");
            cache.inflight.remove(&representative_id);
            match &computed {
                Ok(group) => {
                    if let Some(existing_key) = group
                        .exact_representative_ids
                        .iter()
                        .find_map(|id| cache.aliases.get(id).copied())
                    {
                        cache
                            .entries
                            .get(&existing_key)
                            .cloned()
                            .unwrap_or_else(|| group.clone())
                    } else {
                        insert_same_group(&mut cache, group.clone(), &self.evictions);
                        group.clone()
                    }
                }
                Err(_) => Arc::new(SameImageGroup {
                    representative_id,
                    exact_representative_ids: vec![representative_id],
                    asset_ids: vec![representative_id],
                    scores: BTreeMap::from([(representative_id, 1.0)]),
                }),
            }
        };
        let final_result = computed.map(|_| published);
        *inflight
            .result
            .lock()
            .expect("same-image inflight poisoned") = Some(final_result.clone());
        inflight.ready.notify_all();
        final_result.map_err(anyhow::Error::msg)
    }

    pub fn metrics_json(&self) -> Value {
        let cache = self.inner.lock().expect("same-image cache poisoned");
        let hits = self.hits.load(AtomicOrdering::Relaxed);
        let misses = self.misses.load(AtomicOrdering::Relaxed);
        let compute_count = self.compute_count.load(AtomicOrdering::Relaxed);
        let compute_total_us = self.compute_total_us.load(AtomicOrdering::Relaxed);
        json!({
            "groups": cache.entries.len(),
            "members": cache.member_count,
            "aliases": cache.aliases.len(),
            "hits": hits,
            "misses": misses,
            "hit_rate": if hits + misses == 0 { 0.0 } else { hits as f64 / (hits + misses) as f64 },
            "evictions": self.evictions.load(AtomicOrdering::Relaxed),
            "invalidations": self.invalidations.load(AtomicOrdering::Relaxed),
            "inflight": cache.inflight.len(),
            "max_groups": SAME_CACHE_MAX_GROUPS,
            "max_members": SAME_CACHE_MAX_MEMBERS,
            "max_parallel": SAME_GROUP_PARALLELISM,
            "compute_count": compute_count,
            "compute_avg_ms": if compute_count == 0 { 0.0 } else { compute_total_us as f64 / compute_count as f64 / 1000.0 },
            "compute_last_ms": self.compute_last_us.load(AtomicOrdering::Relaxed) as f64 / 1000.0,
            "compute_max_ms": self.compute_max_us.load(AtomicOrdering::Relaxed) as f64 / 1000.0,
            "vector_index": self.vector_index.metrics_json()
        })
    }
}

fn touch_lru(lru: &mut VecDeque<i64>, key: i64) {
    if let Some(position) = lru.iter().position(|value| *value == key) {
        lru.remove(position);
    }
    lru.push_back(key);
}

fn insert_same_group(
    cache: &mut SameCacheInner,
    group: Arc<SameImageGroup>,
    evictions: &AtomicU64,
) {
    let key = group.key();
    cache.member_count += group.asset_ids.len();
    for representative in &group.exact_representative_ids {
        cache.aliases.insert(*representative, key);
    }
    for asset_id in &group.asset_ids {
        cache.aliases.insert(*asset_id, key);
    }
    cache.entries.insert(key, group);
    touch_lru(&mut cache.lru, key);
    while cache.entries.len() > SAME_CACHE_MAX_GROUPS || cache.member_count > SAME_CACHE_MAX_MEMBERS
    {
        let Some(oldest) = cache.lru.pop_front() else {
            break;
        };
        let Some(removed) = cache.entries.remove(&oldest) else {
            continue;
        };
        cache.member_count = cache.member_count.saturating_sub(removed.asset_ids.len());
        cache.aliases.retain(|_, key| *key != oldest);
        evictions.fetch_add(1, AtomicOrdering::Relaxed);
    }
}

fn manifest_identity(manifest: &Path, account: &str) -> anyhow::Result<String> {
    fn file_stamp(path: &Path) -> String {
        let Ok(metadata) = path.metadata() else {
            return "missing".to_string();
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        format!("{}:{modified}", metadata.len())
    }

    let mut wal_path = manifest.as_os_str().to_os_string();
    wal_path.push("-wal");
    let wal_path = PathBuf::from(wal_path);
    let con = Connection::open_with_flags(
        manifest,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let overview_identity = con
        .query_row(
            r#"select active_asset_count,grouped_asset_count,embedding_row_count
               from image_index_overview_cache where id=1 and schema_version=1"#,
            [],
            |row| {
                Ok(format!(
                    "{}:{}:{}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?
                ))
            },
        )
        .optional()?;
    let content_generation: i64 = con.query_row(
        "select generation from image_index_content_state where id=1",
        [],
        |row| row.get(0),
    )?;
    let (max_asset_id, max_group_asset_id, max_embedding_rowid): (i64, i64, i64) = con.query_row(
        r#"select
                   coalesce((select max(id) from image_assets),0),
                   coalesce((select max(asset_id) from image_exact_groups),0),
                   coalesce((select max(rowid) from image_embeddings),0)"#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    // Identity checks run on every cold same-image lookup. Counting millions
    // of assets/vectors here made a 61 GiB manifest block interactive pages
    // for tens of seconds. The materialized overview provides stable static
    // counts without being invalidated by unrelated occurrence-analysis
    // writes. `content_generation` changes for vector or exact-membership
    // mutations, including replacement of a non-final vector when row counts
    // and maximum rowids remain unchanged. While an overview is absent (tests
    // or an index rebuild), file stamps retain conservative invalidation.
    let (model, dim, updated) = con
        .query_row(
            r#"select model,dim,updated_at from image_embeddings
               not indexed where kind='sscd' order by rowid desc limit 1"#,
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .unwrap_or_else(|| (String::new(), 0, String::new()));
    let structural_identity =
        format!("{content_generation}:{max_asset_id}:{max_group_asset_id}:{max_embedding_rowid}");
    let manifest_generation = overview_identity
        .map(|identity| format!("{identity}:{structural_identity}"))
        .unwrap_or_else(|| {
            format!(
                "{}:{}:{structural_identity}",
                file_stamp(manifest),
                file_stamp(&wal_path)
            )
        });
    Ok(format!(
        "{account}:{model}:{dim}:{updated}:{manifest_generation}"
    ))
}

fn compute_same_image_group(
    manifest: &Path,
    representative_id: i64,
    vector_index: &SscdVectorIndex,
) -> anyhow::Result<SameImageGroup> {
    let trace_timings = std::env::var_os("QQ_ANALYZER_TRACE_SAME_IMAGE").is_some();
    let total_started = Instant::now();
    let open_started = Instant::now();
    let con = Connection::open_with_flags(
        manifest,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let open_ms = open_started.elapsed().as_secs_f64() * 1000.0;
    let representative_started = Instant::now();
    let representative_id = con
        .query_row(
            "select representative_asset_id from image_exact_groups where asset_id=?1",
            [representative_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(representative_id);
    let representative_ms = representative_started.elapsed().as_secs_f64() * 1000.0;
    let query_started = Instant::now();
    let query = query_sscd_vector(&con, representative_id)?;
    let query_ms = query_started.elapsed().as_secs_f64() * 1000.0;
    let mut scores = BTreeMap::<i64, f64>::from([(representative_id, 1.0)]);
    let ann_started = Instant::now();
    if let Some(query) = query {
        let ann_candidates = vector_index.search(
            &query.model,
            query.dim as usize,
            &query.vector,
            SAME_ANN_CANDIDATE_LIMIT,
        )?;
        if let Some(candidates) = ann_candidates {
            for candidate in candidates {
                let score = candidate.score;
                if score + f64::EPSILON < SAME_IMAGE_THRESHOLD {
                    continue;
                }
                scores
                    .entry(candidate.representative_asset_id)
                    .and_modify(|current| *current = (*current).max(score))
                    .or_insert(score);
            }
        } else {
            for (path, blob) in same_image_candidates(&con, representative_id, &query)? {
                if let Some(candidate) = con
                    .query_row(
                        r#"select g.representative_asset_id
                           from image_assets a
                           join image_exact_groups g on g.asset_id=a.id
                           where a.path=?1 and a.stale=0"#,
                        [path],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?
                {
                    let Some(score) = dot_blob(&query.vector, &blob) else {
                        continue;
                    };
                    if score + f64::EPSILON < SAME_IMAGE_THRESHOLD {
                        continue;
                    }
                    scores
                        .entry(candidate)
                        .and_modify(|current| *current = (*current).max(score))
                        .or_insert(score);
                }
            }
        }
    }
    let ann_ms = ann_started.elapsed().as_secs_f64() * 1000.0;
    let exact_representative_ids = scores.keys().copied().collect::<Vec<_>>();
    let placeholders = sql_placeholders(exact_representative_ids.len(), 1);
    let mut stmt = con.prepare(&format!(
        r#"select a.id
           from image_exact_groups g
           cross join image_assets a on a.id=g.asset_id
           where g.representative_asset_id in ({placeholders}) and a.stale=0
           order by a.id"#
    ))?;
    let values = exact_representative_ids
        .iter()
        .copied()
        .map(SqlValue::Integer)
        .collect::<Vec<_>>();
    let expansion_started = Instant::now();
    let asset_ids = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expansion_ms = expansion_started.elapsed().as_secs_f64() * 1000.0;
    let dynamic_started = Instant::now();
    let representative_id =
        dynamic_representative(&con, &exact_representative_ids)?.unwrap_or(representative_id);
    let dynamic_ms = dynamic_started.elapsed().as_secs_f64() * 1000.0;
    if trace_timings {
        eprintln!(
            "same-image timing input={} exact_groups={} assets={} open_ms={open_ms:.3} representative_ms={representative_ms:.3} query_ms={query_ms:.3} ann_ms={ann_ms:.3} expansion_ms={expansion_ms:.3} dynamic_ms={dynamic_ms:.3} total_ms={:.3}",
            representative_id,
            exact_representative_ids.len(),
            asset_ids.len(),
            total_started.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(SameImageGroup {
        representative_id,
        exact_representative_ids,
        asset_ids,
        scores,
    })
}

struct QueryVector {
    model: String,
    dim: i64,
    vector: Vec<f32>,
    bucket: Option<i64>,
    phash: String,
}

fn query_sscd_vector(
    con: &Connection,
    representative_id: i64,
) -> anyhow::Result<Option<QueryVector>> {
    let row = con
        .query_row(
            r#"select e.model,e.dim,e.vec,e.bucket12,coalesce(a.phash_hex,'')
               from image_exact_groups g
               cross join image_assets a on a.id=g.asset_id and a.stale=0
               cross join image_embeddings e on e.path=a.path and e.kind='sscd'
               where g.representative_asset_id=?1
               order by
                   case when a.error is null or a.error='' then 0 else 1 end,
                   case when a.quality_flags not like '%thumbnail%'
                              and a.quality_flags not like '%tiny%'
                              and a.quality_flags not like '%small%'
                              and a.quality_flags not like '%blurry%'
                        then 0 else 1 end,
                   coalesce(a.width,0)*coalesce(a.height,0) desc,
                   a.file_size desc,a.id
               limit 1"#,
            [representative_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((model, dim, blob, bucket, phash)) = row else {
        return Ok(None);
    };
    let Some(vector) = decode_vector(&blob, dim) else {
        return Ok(None);
    };
    Ok(Some(QueryVector {
        model,
        dim,
        vector,
        bucket,
        phash,
    }))
}

fn same_image_candidates(
    con: &Connection,
    representative_id: i64,
    query: &QueryVector,
) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut candidates = HashMap::<String, Vec<u8>>::new();
    if let Some(bucket) = query.bucket {
        let mut stmt = con.prepare(
            r#"select path,vec from image_embeddings
               where kind='sscd' and model=?1 and dim=?2 and bucket12=?3
               limit ?4"#,
        )?;
        let rows = stmt.query_map(
            params![query.model, query.dim, bucket, SAME_CANDIDATE_LIMIT as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?;
        for row in rows {
            let (path, blob) = row?;
            candidates.insert(path, blob);
        }
    }
    if !query.phash.is_empty() {
        let mut stmt = con.prepare(
            r#"select e.path,e.vec
               from image_assets a indexed by image_assets_phash_idx
               join image_embeddings e on e.path=a.path
               where a.stale=0 and a.phash_hex=?1 and e.kind='sscd'
                 and e.model=?2 and e.dim=?3
               limit ?4"#,
        )?;
        for row in stmt.query_map(
            params![
                query.phash,
                query.model,
                query.dim,
                SAME_CANDIDATE_LIMIT as i64
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )? {
            let (path, blob) = row?;
            candidates.insert(path, blob);
        }
    }
    let mut stmt = con.prepare(
        r#"select distinct e.path,e.vec
           from image_exact_groups query_group
           join image_asset_tile_hashes query_tile on query_tile.asset_id=query_group.asset_id
           join image_asset_tile_hashes candidate_tile
             on candidate_tile.tile_algo=query_tile.tile_algo
            and candidate_tile.hash_hex=query_tile.hash_hex
           join image_assets candidate on candidate.id=candidate_tile.asset_id and candidate.stale=0
           join image_embeddings e on e.path=candidate.path and e.kind='sscd'
           where query_group.representative_asset_id=?1
             and e.model=?2 and e.dim=?3
           limit ?4"#,
    )?;
    for row in stmt.query_map(
        params![
            representative_id,
            query.model,
            query.dim,
            SAME_CANDIDATE_LIMIT as i64
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )? {
        let (path, blob) = row?;
        candidates.insert(path, blob);
    }
    Ok(candidates.into_iter().collect())
}

fn decode_vector(blob: &[u8], dim: i64) -> Option<Vec<f32>> {
    if dim <= 0 || blob.len() != dim as usize * 4 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

fn dot_blob(query: &[f32], blob: &[u8]) -> Option<f64> {
    if blob.len() != query.len() * 4 {
        return None;
    }
    Some(
        query
            .iter()
            .zip(blob.chunks_exact(4))
            .map(|(left, right)| {
                *left as f64 * f32::from_le_bytes([right[0], right[1], right[2], right[3]]) as f64
            })
            .sum(),
    )
}

fn dynamic_representative(
    con: &Connection,
    representatives: &[i64],
) -> anyhow::Result<Option<i64>> {
    if representatives.is_empty() {
        return Ok(None);
    }
    let placeholders = sql_placeholders(representatives.len(), 1);
    let sql = format!(
        r#"select a.id
           from image_exact_groups g
           join image_assets a on a.id=g.asset_id
           where g.representative_asset_id in ({placeholders}) and a.stale=0
           order by
               case when a.error is null or a.error='' then 0 else 1 end,
               case when a.quality_flags not like '%thumbnail%'
                          and a.quality_flags not like '%tiny%'
                          and a.quality_flags not like '%small%'
                          and a.quality_flags not like '%blurry%'
                    then 0 else 1 end,
               coalesce(a.width,0)*coalesce(a.height,0) desc,
               a.file_size desc,a.id
           limit 1"#
    );
    let values = representatives
        .iter()
        .copied()
        .map(SqlValue::Integer)
        .collect::<Vec<_>>();
    Ok(con
        .query_row(&sql, rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, i64>(0)
        })
        .optional()?)
}

fn sql_placeholders(count: usize, first: usize) -> String {
    (0..count)
        .map(|index| format!("?{}", first + index))
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Clone, Debug)]
struct CubeRow {
    representative_id: i64,
    date: String,
    sender: String,
    table: String,
    conversation_type: String,
    count: i64,
}

pub fn overview(root: &Path, account: &str, query: &InsightsQuery) -> anyhow::Result<Value> {
    let con = open_manifest(root, account)?;
    if query.from.is_none()
        && query.to.is_none()
        && query.senders.is_empty()
        && query.tables.is_empty()
        && query.conversation_types.is_empty()
    {
        if let Some(value) = overview_from_summaries(&con, account, query)? {
            return Ok(value);
        }
    }
    overview_from_cube_sql(&con, account, query)
}

fn overview_from_cube_sql(
    con: &Connection,
    account: &str,
    query: &InsightsQuery,
) -> anyhow::Result<Value> {
    let archive = con
        .query_row(
            r#"select first_date,last_date
               from image_popularity_summary_cache
               where scope='all' and scope_key=''"#,
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .unwrap_or_default();

    let mut total_timeline = BTreeMap::<String, i64>::new();
    {
        let mut stmt = con.prepare(
            r#"select scope_key,reference_count
               from image_popularity_summary_cache
               where scope='date' order by scope_key"#,
        )?;
        for row in stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (date, count) = row?;
            total_timeline.insert(date, count);
        }
    }

    let (selected_clauses, selected_values) = cube_filter_parts(query, true, true);
    let selected_source = overview_cube_source(query, true, true);
    let (selected_references, selected_assets, selected_senders, selected_tables) =
        if let Some(summary) = cached_single_filter_summary(con, query)? {
            summary
        } else {
            let summary_sql = format!(
                r#"select coalesce(sum(reference_count),0),
                          count(distinct representative_asset_id),
                          count(distinct sender_uin),
                          count(distinct conversation_table)
                   from {selected_source} where {}"#,
                selected_clauses.join(" and ")
            );
            con.query_row(
                &summary_sql,
                rusqlite::params_from_iter(selected_values.iter()),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?
        };

    let timeline_sql = format!(
        r#"select message_date,sum(reference_count)
           from {selected_source}
           where {}
           group by message_date order by message_date"#,
        selected_clauses.join(" and ")
    );
    let mut selected_timeline = BTreeMap::<String, i64>::new();
    {
        let mut stmt = con.prepare(&timeline_sql)?;
        for row in stmt.query_map(rusqlite::params_from_iter(selected_values.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (date, count) = row?;
            selected_timeline.insert(date, count);
        }
    }
    for (date, count) in &selected_timeline {
        total_timeline.entry(date.clone()).or_insert(*count);
    }
    let timeline = total_timeline
        .into_iter()
        .map(|(date, count)| {
            json!({
                "date": date,
                "count": count,
                "selected_count": selected_timeline.get(&date).copied().unwrap_or(0)
            })
        })
        .collect::<Vec<_>>();

    let senders = if query.from.is_none()
        && query.to.is_none()
        && query.tables.is_empty()
        && query.conversation_types.is_empty()
    {
        let mut stmt = con.prepare(
            r#"select scope_key,reference_count
               from image_popularity_summary_cache
               where scope='sender'
               order by reference_count desc,scope_key limit 100"#,
        )?;
        let values = stmt
            .query_map([], |row| {
                let uin = row.get::<_, String>(0)?;
                Ok(json!({
                    "uin": uin,
                    "label": uin,
                    "count": row.get::<_, i64>(1)?,
                    "selected": query.senders.contains(&uin)
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        values
    } else {
        let (clauses, values) = cube_filter_parts(query, false, true);
        let source = overview_cube_source(query, false, true);
        let sql = format!(
            r#"select sender_uin,sum(reference_count) as total_references
               from {source} where {}
               group by sender_uin
               order by total_references desc,sender_uin limit 100"#,
            clauses.join(" and ")
        );
        let mut stmt = con.prepare(&sql)?;
        let facets = stmt
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                let uin = row.get::<_, String>(0)?;
                Ok(json!({
                    "uin": uin,
                    "label": uin,
                    "count": row.get::<_, i64>(1)?,
                    "selected": query.senders.contains(&uin)
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        facets
    };

    let conversations = if query.from.is_none()
        && query.to.is_none()
        && query.senders.is_empty()
        && query.conversation_types.is_empty()
    {
        let mut stmt = con.prepare(
            r#"select scope_key,reference_count
               from image_popularity_summary_cache
               where scope='conversation'
               order by reference_count desc,scope_key limit 100"#,
        )?;
        let facets = stmt
            .query_map([], |row| {
                let table = row.get::<_, String>(0)?;
                let (conversation_type, id) = split_table(&table);
                Ok(json!({
                    "table": table,
                    "id": id,
                    "type": conversation_type,
                    "label": table,
                    "count": row.get::<_, i64>(1)?,
                    "selected": query.tables.contains(&table)
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        facets
    } else if query.senders.is_empty() && daily_conversation_cache_ready(con)? {
        let mut clauses = vec!["1=1".to_string()];
        let mut values = Vec::<SqlValue>::new();
        push_text_filter(
            &mut clauses,
            &mut values,
            "message_date>=?",
            query.from.as_deref(),
        );
        push_text_filter(
            &mut clauses,
            &mut values,
            "message_date<=?",
            query.to.as_deref(),
        );
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_type",
            &query.conversation_types,
        );
        let sql = format!(
            r#"select conversation_table,conversation_type,
                      sum(reference_count) as total_references
               from image_popularity_daily_conversation where {}
               group by conversation_table,conversation_type
               order by total_references desc,conversation_table limit 100"#,
            clauses.join(" and ")
        );
        let mut stmt = con.prepare(&sql)?;
        let facets = stmt
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                let table = row.get::<_, String>(0)?;
                let conversation_type = row.get::<_, String>(1)?;
                let (_, id) = split_table(&table);
                Ok(json!({
                    "table": table,
                    "id": id,
                    "type": conversation_type,
                    "label": table,
                    "count": row.get::<_, i64>(2)?,
                    "selected": query.tables.contains(&table)
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        facets
    } else {
        let (clauses, values) = cube_filter_parts(query, true, false);
        let source = overview_cube_source(query, true, false);
        let sql = format!(
            r#"select conversation_table,conversation_type,
                      sum(reference_count) as total_references
               from {source} where {}
               group by conversation_table,conversation_type
               order by total_references desc,conversation_table limit 100"#,
            clauses.join(" and ")
        );
        let mut stmt = con.prepare(&sql)?;
        let facets = stmt
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                let table = row.get::<_, String>(0)?;
                let conversation_type = row.get::<_, String>(1)?;
                let (_, id) = split_table(&table);
                Ok(json!({
                    "table": table,
                    "id": id,
                    "type": conversation_type,
                    "label": table,
                    "count": row.get::<_, i64>(2)?,
                    "selected": query.tables.contains(&table)
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        facets
    };

    Ok(json!({
        "account": account,
        "coverage": build_state_json(con)?,
        "range": {"from": archive.0, "to": archive.1},
        "summary": {
            "references": selected_references,
            "images": selected_assets,
            "senders": selected_senders,
            "conversations": selected_tables
        },
        "filters": query,
        "timeline": timeline,
        "senders": senders,
        "conversations": conversations
    }))
}

fn daily_conversation_cache_ready(con: &Connection) -> anyhow::Result<bool> {
    Ok(popularity_build_state(con)?.is_some_and(|state| {
        state.schema_version == POPULARITY_SCHEMA_VERSION && state.summary_ready
    }))
}

type OverviewSummary = (i64, i64, i64, i64);

fn cached_single_filter_summary(
    con: &Connection,
    query: &InsightsQuery,
) -> anyhow::Result<Option<OverviewSummary>> {
    let scope_and_key = if query.from.is_none()
        && query.to.is_none()
        && query.senders.len() == 1
        && query.tables.is_empty()
        && query.conversation_types.is_empty()
    {
        Some(("sender", query.senders[0].as_str()))
    } else if query.from.is_none()
        && query.to.is_none()
        && query.senders.is_empty()
        && query.tables.len() == 1
        && query.conversation_types.is_empty()
    {
        Some(("conversation", query.tables[0].as_str()))
    } else if query.senders.is_empty()
        && query.tables.is_empty()
        && query.conversation_types.is_empty()
        && query.from.is_some()
        && query.from == query.to
    {
        Some(("date", query.from.as_deref().unwrap_or_default()))
    } else {
        None
    };
    let Some((scope, key)) = scope_and_key else {
        return Ok(None);
    };
    Ok(con
        .query_row(
            r#"select reference_count,image_count,sender_count,conversation_count
               from image_popularity_summary_cache
               where scope=?1 and scope_key=?2"#,
            params![scope, key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?)
}

fn overview_from_summaries(
    con: &Connection,
    account: &str,
    query: &InsightsQuery,
) -> anyhow::Result<Option<Value>> {
    let all = con
        .query_row(
            r#"select reference_count,image_count,sender_count,conversation_count,
                      first_date,last_date
               from image_popularity_summary_cache
               where scope='all' and scope_key=''"#,
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((references, images, senders_count, conversations_count, from, to)) = all else {
        return Ok(None);
    };
    let mut stmt = con.prepare(
        r#"select scope_key,reference_count
           from image_popularity_summary_cache
           where scope='date' order by scope_key"#,
    )?;
    let timeline = stmt
        .query_map([], |row| {
            let date = row.get::<_, String>(0)?;
            let count = row.get::<_, i64>(1)?;
            Ok(json!({"date": date, "count": count, "selected_count": count}))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = con.prepare(
        r#"select scope_key,reference_count
           from image_popularity_summary_cache
           where scope='sender'
           order by reference_count desc,scope_key limit 100"#,
    )?;
    let senders = stmt
        .query_map([], |row| {
            let uin = row.get::<_, String>(0)?;
            Ok(json!({
                "uin": uin,
                "label": uin,
                "count": row.get::<_, i64>(1)?,
                "selected": false
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = con.prepare(
        r#"select scope_key,reference_count
           from image_popularity_summary_cache
           where scope='conversation'
           order by reference_count desc,scope_key limit 100"#,
    )?;
    let conversations = stmt
        .query_map([], |row| {
            let table = row.get::<_, String>(0)?;
            let (conversation_type, id) = split_table(&table);
            Ok(json!({
                "table": table,
                "id": id,
                "type": conversation_type,
                "label": table,
                "count": row.get::<_, i64>(1)?,
                "selected": false
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(json!({
        "account": account,
        "coverage": build_state_json(con)?,
        "range": {"from": from, "to": to},
        "summary": {
            "references": references,
            "images": images,
            "senders": senders_count,
            "conversations": conversations_count
        },
        "filters": query,
        "timeline": timeline,
        "senders": senders,
        "conversations": conversations
    })))
}

fn cube_filter_parts(
    query: &InsightsQuery,
    apply_senders: bool,
    apply_tables: bool,
) -> (Vec<String>, Vec<SqlValue>) {
    let mut clauses = vec!["1=1".to_string()];
    let mut values = Vec::<SqlValue>::new();
    push_text_filter(
        &mut clauses,
        &mut values,
        "message_date>=?",
        query.from.as_deref(),
    );
    push_text_filter(
        &mut clauses,
        &mut values,
        "message_date<=?",
        query.to.as_deref(),
    );
    if apply_senders {
        push_in_filter(&mut clauses, &mut values, "sender_uin", &query.senders);
    }
    if apply_tables {
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_table",
            &query.tables,
        );
    }
    push_in_filter(
        &mut clauses,
        &mut values,
        "conversation_type",
        &query.conversation_types,
    );
    (clauses, values)
}

fn overview_cube_source(
    query: &InsightsQuery,
    apply_senders: bool,
    apply_tables: bool,
) -> &'static str {
    if apply_senders && !query.senders.is_empty() {
        "image_popularity_cube indexed by image_popularity_cube_sender_cover_idx"
    } else if apply_tables && !query.tables.is_empty() {
        "image_popularity_cube indexed by image_popularity_cube_conversation_cover_idx"
    } else {
        "image_popularity_cube indexed by image_popularity_cube_date_cover_idx"
    }
}

fn build_state_json(con: &Connection) -> anyhow::Result<Value> {
    let (occurrences, facts, cube_rows, ready, refreshed): (i64, i64, i64, bool, String) = con
        .query_row(
            r#"select occurrence_count,fact_count,cube_row_count,summary_ready,refreshed_at
               from image_popularity_build_state where id=1"#,
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
        .optional()?
        .unwrap_or((0, 0, 0, false, String::new()));
    Ok(json!({
        "ready": ready,
        "occurrences": occurrences,
        "facts": facts,
        "cube_rows": cube_rows,
        "fact_coverage": if occurrences == 0 { 0.0 } else { facts as f64 / occurrences as f64 },
        "updated_at": refreshed
    }))
}

fn open_manifest(root: &Path, account: &str) -> anyhow::Result<Connection> {
    let path = image_index::default_manifest_path(root, account);
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

pub fn overview_cache_identity(root: &Path, account: &str) -> anyhow::Result<String> {
    let con = open_manifest(root, account)?;
    if let Some(state) = popularity_build_state(&con)? {
        return Ok(format!(
            "{}:{}:{}:{}:{}:{}",
            state.schema_version,
            state.occurrence_count,
            state.fact_count,
            state.cube_row_count,
            state.summary_ready,
            state.refreshed_at
        ));
    }
    let metadata = image_index::default_manifest_path(root, account).metadata()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    Ok(format!("unbuilt:{}:{modified}", metadata.len()))
}

fn cube_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CubeRow> {
    Ok(CubeRow {
        representative_id: row.get(0)?,
        date: row.get(1)?,
        sender: row.get(2)?,
        table: row.get(3)?,
        conversation_type: row.get(4)?,
        count: row.get(5)?,
    })
}

#[cfg(test)]
fn matches_filters(
    row: &CubeRow,
    query: &InsightsQuery,
    apply_senders: bool,
    apply_tables: bool,
) -> bool {
    if query.from.as_ref().is_some_and(|from| row.date < *from)
        || query.to.as_ref().is_some_and(|to| row.date > *to)
    {
        return false;
    }
    if apply_senders && !query.senders.is_empty() && !query.senders.contains(&row.sender) {
        return false;
    }
    if apply_tables && !query.tables.is_empty() && !query.tables.contains(&row.table) {
        return false;
    }
    query.conversation_types.is_empty() || query.conversation_types.contains(&row.conversation_type)
}

#[derive(Clone, Debug, Serialize)]
pub struct RankedAsset {
    pub id: i64,
    pub exact_representative_ids: Vec<i64>,
    pub reference_count: i64,
    pub sender_count: usize,
    pub conversation_count: usize,
    pub first_date: String,
    pub peak_date: String,
    pub last_date: String,
    pub trend: Vec<Value>,
    pub same_image_members: usize,
    pub growth: i64,
    pub growth_rate: Option<f64>,
    pub thumbnail_url: String,
    pub content_url: String,
    pub detail_url: String,
    pub references_url: String,
}

pub fn compute_ranked_assets<F>(
    root: &Path,
    account: &str,
    query: &InsightsQuery,
    same_cache: &SameImageCache,
    cancel: Option<&AtomicBool>,
    mut progress: F,
) -> anyhow::Result<Vec<RankedAsset>>
where
    F: FnMut(usize, usize),
{
    if is_ranking_cancelled(cancel) {
        anyhow::bail!("ranking task cancelled");
    }
    let con = open_manifest(root, account)?;
    let archive_range = archive_range(&con)?;
    let (current_from, current_to, previous_from, previous_to) =
        ranking_windows(query, archive_range.as_ref())?;
    let candidates =
        candidate_representatives(&con, query, current_from.as_deref(), current_to.as_deref())?;
    let total = candidates.len();
    let mut groups = HashMap::<i64, Arc<SameImageGroup>>::new();
    let mut representative_to_group = HashMap::<i64, i64>::new();
    progress(0, total);
    let resolved = resolve_candidates_parallel(
        root,
        account,
        &candidates,
        same_cache,
        cancel,
        |processed| {
            progress(processed, total);
        },
    )?;
    if is_ranking_cancelled(cancel) {
        anyhow::bail!("ranking task cancelled");
    }
    for group in resolved {
        let key = group.key();
        for member in &group.exact_representative_ids {
            representative_to_group.insert(*member, key);
        }
        groups.entry(key).or_insert(group);
    }
    let members = representative_to_group.keys().copied().collect::<Vec<_>>();
    let relevant_from = [current_from.as_deref(), previous_from.as_deref()]
        .into_iter()
        .flatten()
        .min();
    let relevant_to = [current_to.as_deref(), previous_to.as_deref()]
        .into_iter()
        .flatten()
        .max();
    let rows =
        load_cube_rows_for_representatives(&con, &members, query, relevant_from, relevant_to)?;
    let extrema =
        load_cube_extrema_for_representatives(&con, &members, query, current_from.as_deref())?;
    let mut first_date_by_group = HashMap::<i64, String>::new();
    let mut last_before_by_group = HashMap::<i64, String>::new();
    for (representative, (first_date, last_before)) in extrema {
        let Some(group) = representative_to_group.get(&representative).copied() else {
            continue;
        };
        first_date_by_group
            .entry(group)
            .and_modify(|existing| {
                if first_date < *existing {
                    *existing = first_date.clone();
                }
            })
            .or_insert(first_date);
        if let Some(last_before) = last_before {
            last_before_by_group
                .entry(group)
                .and_modify(|existing| {
                    if last_before > *existing {
                        *existing = last_before.clone();
                    }
                })
                .or_insert(last_before);
        }
    }
    let mut rows_by_group = HashMap::<i64, Vec<CubeRow>>::new();
    for row in rows {
        if let Some(group) = representative_to_group.get(&row.representative_id) {
            rows_by_group.entry(*group).or_default().push(row);
        }
    }
    let mut ranked = Vec::new();
    for (key, group) in groups {
        let group_rows = rows_by_group.remove(&key).unwrap_or_default();
        if let Some(card) = rank_group(
            &group,
            &group_rows,
            query,
            current_from.as_deref(),
            current_to.as_deref(),
            previous_from.as_deref(),
            previous_to.as_deref(),
            first_date_by_group.get(&key).map(String::as_str),
            last_before_by_group.get(&key).map(String::as_str),
        ) {
            ranked.push(card);
        }
    }
    ranked.sort_by(|left, right| ranked_cmp(left, right, &query.rank));
    Ok(ranked)
}

fn resolve_candidates_parallel<F>(
    root: &Path,
    account: &str,
    candidates: &[i64],
    same_cache: &SameImageCache,
    cancel: Option<&AtomicBool>,
    mut progress: F,
) -> anyhow::Result<Vec<Arc<SameImageGroup>>>
where
    F: FnMut(usize),
{
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let next = AtomicU64::new(0);
    let (sender, receiver) =
        std::sync::mpsc::channel::<(usize, Result<Arc<SameImageGroup>, String>)>();
    let mut resolved = Vec::with_capacity(candidates.len());
    resolved.resize_with(candidates.len(), || None);
    std::thread::scope(|scope| -> anyhow::Result<()> {
        for _ in 0..SAME_GROUP_PARALLELISM.min(candidates.len()) {
            let sender = sender.clone();
            let next = &next;
            scope.spawn(move || loop {
                if is_ranking_cancelled(cancel) {
                    break;
                }
                let index = next.fetch_add(1, AtomicOrdering::Relaxed) as usize;
                let Some(&representative) = candidates.get(index) else {
                    break;
                };
                let result = same_cache
                    .resolve(root, account, representative)
                    .map_err(|error| error.to_string());
                if sender.send((index, result)).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        for (processed, (index, result)) in receiver.into_iter().enumerate() {
            resolved[index] = Some(result);
            progress(processed + 1);
        }
        Ok(())
    })?;
    if is_ranking_cancelled(cancel) {
        anyhow::bail!("ranking task cancelled");
    }
    resolved
        .into_iter()
        .map(|result| {
            result
                .expect("every same-image candidate has a worker result")
                .map_err(anyhow::Error::msg)
        })
        .collect()
}

fn is_ranking_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(AtomicOrdering::Relaxed))
}

fn archive_range(con: &Connection) -> anyhow::Result<Option<(String, String)>> {
    if let Some(range) = con
        .query_row(
            r#"select first_date,last_date
               from image_popularity_summary_cache
               where scope='all' and scope_key=''"#,
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        return Ok(Some(range));
    }
    let (from, to) = con.query_row(
        "select min(message_date),max(message_date) from image_popularity_cube",
        [],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    )?;
    Ok(from.zip(to))
}

type RankingWindows = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn ranking_windows(
    query: &InsightsQuery,
    archive_range: Option<&(String, String)>,
) -> anyhow::Result<RankingWindows> {
    let mut current_from = query.from.clone();
    let mut current_to = query.to.clone();
    if matches!(query.rank.as_str(), "growth" | "revival") {
        if current_from.is_none() && current_to.is_none() {
            if let Some((_, archive_to)) = archive_range {
                let to = NaiveDate::parse_from_str(archive_to, "%Y-%m-%d")?;
                current_to = Some(to.to_string());
                current_from =
                    Some((to - ChronoDuration::days(query.window.saturating_sub(1))).to_string());
            }
        } else if let Some((archive_from, archive_to)) = archive_range {
            current_from.get_or_insert_with(|| archive_from.clone());
            current_to.get_or_insert_with(|| archive_to.clone());
        }
    }
    let (previous_from, previous_to) = match (&current_from, &current_to) {
        (Some(from), Some(to)) => {
            let from = NaiveDate::parse_from_str(from, "%Y-%m-%d")?;
            let to = NaiveDate::parse_from_str(to, "%Y-%m-%d")?;
            let days = (to - from).num_days() + 1;
            (
                Some((from - ChronoDuration::days(days)).to_string()),
                Some((from - ChronoDuration::days(1)).to_string()),
            )
        }
        _ => (None, None),
    };
    Ok((current_from, current_to, previous_from, previous_to))
}

fn candidate_representatives(
    con: &Connection,
    query: &InsightsQuery,
    current_from: Option<&str>,
    current_to: Option<&str>,
) -> anyhow::Result<Vec<i64>> {
    if current_from.is_none()
        && current_to.is_none()
        && query.senders.is_empty()
        && query.tables.is_empty()
        && query.conversation_types.is_empty()
    {
        let mut stmt = con.prepare(
            r#"select representative_asset_id
               from image_exact_reference_counts
               order by reference_count desc,representative_asset_id desc
               limit ?1"#,
        )?;
        let cached = stmt
            .query_map([CANDIDATE_LIMIT as i64], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !cached.is_empty() {
            return Ok(cached);
        }
    }
    let mut clauses = vec!["1=1".to_string()];
    let mut values = Vec::<SqlValue>::new();
    push_text_filter(&mut clauses, &mut values, "message_date>=?", current_from);
    push_text_filter(&mut clauses, &mut values, "message_date<=?", current_to);
    push_in_filter(&mut clauses, &mut values, "sender_uin", &query.senders);
    push_in_filter(
        &mut clauses,
        &mut values,
        "conversation_table",
        &query.tables,
    );
    push_in_filter(
        &mut clauses,
        &mut values,
        "conversation_type",
        &query.conversation_types,
    );
    let sql = format!(
        r#"select representative_asset_id,sum(reference_count) as total_references
           from {}
           where {}
           group by representative_asset_id
           order by total_references desc,representative_asset_id
           limit {}"#,
        overview_cube_source(query, true, true),
        clauses.join(" and "),
        CANDIDATE_LIMIT
    );
    let mut stmt = con.prepare(&sql)?;
    let representatives = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(representatives)
}

fn push_text_filter(
    clauses: &mut Vec<String>,
    values: &mut Vec<SqlValue>,
    clause: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        clauses.push(clause.to_string());
        values.push(SqlValue::Text(value.to_string()));
    }
}

fn push_in_filter(
    clauses: &mut Vec<String>,
    values: &mut Vec<SqlValue>,
    column: &str,
    selected: &[String],
) {
    if selected.is_empty() {
        return;
    }
    clauses.push(format!(
        "{column} in ({})",
        std::iter::repeat_n("?", selected.len())
            .collect::<Vec<_>>()
            .join(",")
    ));
    values.extend(selected.iter().cloned().map(SqlValue::Text));
}

fn load_cube_rows_for_representatives(
    con: &Connection,
    representatives: &[i64],
    query: &InsightsQuery,
    from: Option<&str>,
    to: Option<&str>,
) -> anyhow::Result<Vec<CubeRow>> {
    let mut rows = Vec::new();
    for chunk in representatives.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = anonymous_placeholders(chunk.len());
        let mut clauses = vec![format!("representative_asset_id in ({placeholders})")];
        let mut values = chunk
            .iter()
            .copied()
            .map(SqlValue::Integer)
            .collect::<Vec<_>>();
        push_text_filter(&mut clauses, &mut values, "message_date>=?", from);
        push_text_filter(&mut clauses, &mut values, "message_date<=?", to);
        push_in_filter(&mut clauses, &mut values, "sender_uin", &query.senders);
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_table",
            &query.tables,
        );
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_type",
            &query.conversation_types,
        );
        let mut stmt = con.prepare(&format!(
            r#"select representative_asset_id,message_date,sender_uin,
                      conversation_table,conversation_type,reference_count
               from image_popularity_cube
               where {}
               order by message_date,representative_asset_id"#,
            clauses.join(" and ")
        ))?;
        rows.extend(
            stmt.query_map(rusqlite::params_from_iter(values.iter()), cube_row)?
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(rows)
}

type CubeExtrema = HashMap<i64, (String, Option<String>)>;

fn load_cube_extrema_for_representatives(
    con: &Connection,
    representatives: &[i64],
    query: &InsightsQuery,
    current_from: Option<&str>,
) -> anyhow::Result<CubeExtrema> {
    let mut extrema = CubeExtrema::new();
    for chunk in representatives.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = anonymous_placeholders(chunk.len());
        let mut clauses = vec![format!("representative_asset_id in ({placeholders})")];
        let mut values = Vec::<SqlValue>::new();
        let last_before_sql = if let Some(current_from) = current_from {
            values.push(SqlValue::Text(current_from.to_string()));
            "max(case when message_date<? then message_date end)"
        } else {
            "null"
        };
        values.extend(chunk.iter().copied().map(SqlValue::Integer));
        push_in_filter(&mut clauses, &mut values, "sender_uin", &query.senders);
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_table",
            &query.tables,
        );
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_type",
            &query.conversation_types,
        );
        let sql = format!(
            r#"select representative_asset_id,min(message_date),{last_before_sql}
               from image_popularity_cube
               where {}
               group by representative_asset_id"#,
            clauses.join(" and ")
        );
        let mut stmt = con.prepare(&sql)?;
        for row in stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })? {
            let (representative, first_date, last_before) = row?;
            extrema.insert(representative, (first_date, last_before));
        }
    }
    Ok(extrema)
}

#[allow(clippy::too_many_arguments)]
fn rank_group(
    group: &SameImageGroup,
    rows: &[CubeRow],
    query: &InsightsQuery,
    current_from: Option<&str>,
    current_to: Option<&str>,
    previous_from: Option<&str>,
    previous_to: Option<&str>,
    historical_first_date: Option<&str>,
    historical_last_before: Option<&str>,
) -> Option<RankedAsset> {
    let dimensions_match = |row: &CubeRow| {
        (query.senders.is_empty() || query.senders.contains(&row.sender))
            && (query.tables.is_empty() || query.tables.contains(&row.table))
            && (query.conversation_types.is_empty()
                || query.conversation_types.contains(&row.conversation_type))
    };
    let in_range = |row: &CubeRow, from: Option<&str>, to: Option<&str>| {
        from.is_none_or(|from| row.date.as_str() >= from)
            && to.is_none_or(|to| row.date.as_str() <= to)
    };
    let current_rows = rows
        .iter()
        .filter(|row| dimensions_match(row) && in_range(row, current_from, current_to))
        .collect::<Vec<_>>();
    let reference_count = current_rows.iter().map(|row| row.count).sum::<i64>();
    if reference_count == 0 {
        return None;
    }
    let sender_count = current_rows
        .iter()
        .map(|row| row.sender.as_str())
        .collect::<HashSet<_>>()
        .len();
    let conversation_count = current_rows
        .iter()
        .map(|row| row.table.as_str())
        .collect::<HashSet<_>>()
        .len();
    let mut daily = BTreeMap::<String, i64>::new();
    for row in &current_rows {
        *daily.entry(row.date.clone()).or_default() += row.count;
    }
    let first_date = historical_first_date
        .or_else(|| {
            rows.iter()
                .filter(|row| dimensions_match(row))
                .map(|row| row.date.as_str())
                .min()
        })
        .unwrap_or_default()
        .to_string();
    let last_date = current_rows
        .iter()
        .map(|row| row.date.as_str())
        .max()
        .unwrap_or_default()
        .to_string();
    let peak_date = daily
        .iter()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
        .map(|(date, _)| date.clone())
        .unwrap_or_default();
    let previous_count = rows
        .iter()
        .filter(|row| dimensions_match(row) && in_range(row, previous_from, previous_to))
        .map(|row| row.count)
        .sum::<i64>();
    let growth = reference_count - previous_count;
    let growth_rate = (previous_count > 0).then_some(growth as f64 / previous_count as f64);
    match query.rank.as_str() {
        "new"
            if current_from.is_some_and(|from| first_date.as_str() < from)
                || current_to.is_some_and(|to| first_date.as_str() > to) =>
        {
            return None;
        }
        "revival" => {
            let Some(from) = current_from else {
                return None;
            };
            let previous_window_count = previous_count;
            let last_before = historical_last_before.or_else(|| {
                rows.iter()
                    .filter(|row| dimensions_match(row) && row.date.as_str() < from)
                    .map(|row| row.date.as_str())
                    .max()
            });
            let dormant_days = last_before
                .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
                .and_then(|last| {
                    NaiveDate::parse_from_str(from, "%Y-%m-%d")
                        .ok()
                        .map(|from| from - last)
                })
                .map(|duration| duration.num_days())
                .unwrap_or(0);
            if reference_count < 5 || previous_window_count != 0 || dormant_days < 30 {
                return None;
            }
        }
        _ => {}
    }
    Some(RankedAsset {
        id: group.representative_id,
        exact_representative_ids: group.exact_representative_ids.clone(),
        reference_count,
        sender_count,
        conversation_count,
        first_date,
        peak_date,
        last_date,
        trend: daily
            .into_iter()
            .map(|(date, count)| json!({"date": date, "count": count}))
            .collect(),
        same_image_members: group.asset_ids.len(),
        growth,
        growth_rate,
        thumbnail_url: format!(
            "/api/image-index/assets/{}/thumbnail?edge=640",
            group.representative_id
        ),
        content_url: format!(
            "/api/image-index/assets/{}/content",
            group.representative_id
        ),
        detail_url: format!("/images/{}", group.representative_id),
        references_url: format!("/images/{}/references", group.representative_id),
    })
}

fn ranked_cmp(left: &RankedAsset, right: &RankedAsset, rank: &str) -> Ordering {
    let ordering = match rank {
        "reach" => right
            .conversation_count
            .cmp(&left.conversation_count)
            .then_with(|| right.sender_count.cmp(&left.sender_count))
            .then_with(|| right.reference_count.cmp(&left.reference_count)),
        "growth" => right
            .growth
            .cmp(&left.growth)
            .then_with(|| right.reference_count.cmp(&left.reference_count)),
        "new" => right
            .first_date
            .cmp(&left.first_date)
            .then_with(|| right.reference_count.cmp(&left.reference_count)),
        "revival" => right
            .reference_count
            .cmp(&left.reference_count)
            .then_with(|| right.growth.cmp(&left.growth)),
        _ => right.reference_count.cmp(&left.reference_count),
    };
    ordering.then_with(|| left.id.cmp(&right.id))
}

pub fn compare(
    root: &Path,
    account: &str,
    query: &InsightsQuery,
    by: &str,
    same_cache: &SameImageCache,
) -> anyhow::Result<Value> {
    if by != "sender" && by != "conversation" {
        anyhow::bail!("compare by must be sender or conversation");
    }
    let con = open_manifest(root, account)?;
    let column = if by == "sender" {
        "sender_uin"
    } else {
        "conversation_table"
    };
    let unfiltered = query.from.is_none()
        && query.to.is_none()
        && query.senders.is_empty()
        && query.tables.is_empty()
        && query.conversation_types.is_empty();
    let dimensions = if unfiltered {
        let mut stmt = con.prepare(
            r#"select scope_key,reference_count
               from image_popularity_summary_cache
               where scope=?1
               order by reference_count desc,scope_key
               limit 30"#,
        )?;
        let rows = stmt
            .query_map(
                [if by == "sender" {
                    "sender"
                } else {
                    "conversation"
                }],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    } else {
        let mut clauses = vec!["1=1".to_string()];
        let mut values = Vec::<SqlValue>::new();
        push_text_filter(
            &mut clauses,
            &mut values,
            "message_date>=?",
            query.from.as_deref(),
        );
        push_text_filter(
            &mut clauses,
            &mut values,
            "message_date<=?",
            query.to.as_deref(),
        );
        push_in_filter(&mut clauses, &mut values, "sender_uin", &query.senders);
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_table",
            &query.tables,
        );
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_type",
            &query.conversation_types,
        );
        let sql = format!(
            r#"select {column},sum(reference_count)
               from {}
               where {}
               group by {column}
               order by sum(reference_count) desc,{column}
               limit 30"#,
            overview_cube_source(query, true, true),
            clauses.join(" and ")
        );
        let mut stmt = con.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let candidates =
        candidate_representatives(&con, query, query.from.as_deref(), query.to.as_deref())?;
    let dimension_keys = dimensions
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let mut candidate_assets = HashMap::<String, HashMap<i64, i64>>::new();
    if !candidates.is_empty() && !dimension_keys.is_empty() {
        let mut values = candidates
            .iter()
            .copied()
            .map(SqlValue::Integer)
            .collect::<Vec<_>>();
        let mut clauses = vec![format!(
            "representative_asset_id in ({})",
            anonymous_placeholders(candidates.len())
        )];
        push_text_filter(
            &mut clauses,
            &mut values,
            "message_date>=?",
            query.from.as_deref(),
        );
        push_text_filter(
            &mut clauses,
            &mut values,
            "message_date<=?",
            query.to.as_deref(),
        );
        push_in_filter(&mut clauses, &mut values, "sender_uin", &query.senders);
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_table",
            &query.tables,
        );
        push_in_filter(
            &mut clauses,
            &mut values,
            "conversation_type",
            &query.conversation_types,
        );
        push_in_filter(&mut clauses, &mut values, column, &dimension_keys);
        let source = if !query.senders.is_empty()
            || !query.tables.is_empty()
            || query.from.is_some()
            || query.to.is_some()
        {
            overview_cube_source(query, true, true)
        } else {
            "image_popularity_cube indexed by sqlite_autoindex_image_popularity_cube_1"
        };
        let sql = format!(
            r#"select {column},representative_asset_id,sum(reference_count)
               from {source}
               where {}
               group by {column},representative_asset_id"#,
            clauses.join(" and ")
        );
        let mut stmt = con.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(values.iter()))?;
        while let Some(row) = rows.next()? {
            candidate_assets
                .entry(row.get::<_, String>(0)?)
                .or_default()
                .insert(row.get::<_, i64>(1)?, row.get::<_, i64>(2)?);
        }
    }
    let mut items = Vec::new();
    for (key, count) in dimensions {
        let mut assets = candidate_assets
            .remove(&key)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        assets.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let mut grouped_assets = HashMap::<i64, (i64, i64)>::new();
        for (id, references) in assets.into_iter().take(COMPARE_CANDIDATE_LIMIT) {
            let group = same_cache.resolve(root, account, id)?;
            let grouped = grouped_assets
                .entry(group.key())
                .or_insert((group.representative_id, 0));
            grouped.1 += references;
        }
        let mut grouped_assets = grouped_assets.into_values().collect::<Vec<_>>();
        grouped_assets
            .sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let top = grouped_assets
            .into_iter()
            .take(8)
            .map(|(id, references)| {
                json!({
                    "id": id,
                    "references": references,
                    "thumbnail_url": format!("/api/image-index/assets/{id}/thumbnail?edge=160"),
                    "detail_url": format!("/images/{id}")
                })
            })
            .collect::<Vec<_>>();
        let (conversation_type, id) = if by == "conversation" {
            let (kind, id) = split_table(&key);
            (Some(kind), Some(id))
        } else {
            (None, None)
        };
        items.push(json!({
            "key": key,
            "label": key,
            "type": conversation_type,
            "id": id,
            "references": count,
            "top_images": top
        }));
    }
    Ok(json!({
        "account": account,
        "by": by,
        "ranking_scope": "candidate_pool",
        "candidate_limit": CANDIDATE_LIMIT,
        "exhaustive": false,
        "items": items
    }))
}

fn anonymous_placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Clone)]
pub struct RankingTaskCache {
    inner: Arc<Mutex<RankingTaskInner>>,
    workers: Arc<Semaphore>,
    same_cache: Arc<SameImageCache>,
}

#[derive(Default)]
struct RankingTaskInner {
    sequence: u64,
    tasks: HashMap<String, RankingTask>,
}

struct RankingTask {
    state: RankingTaskState,
    processed: usize,
    total: usize,
    results: Option<Arc<Vec<RankedAsset>>>,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
    sequence: u64,
    request_token: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RankingTaskState {
    Queued,
    Running,
    Complete,
    Cancelled,
    Failed,
}

impl RankingTaskCache {
    pub fn new(same_cache: Arc<SameImageCache>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RankingTaskInner::default())),
            workers: Arc::new(Semaphore::new(2)),
            same_cache,
        }
    }

    pub async fn request(
        &self,
        root: PathBuf,
        account: String,
        query: InsightsQuery,
        cursor: Option<&str>,
        limit: usize,
        request_token: Option<&str>,
    ) -> anyhow::Result<Value> {
        let identity_cache = self.same_cache.clone();
        let identity_root = root.clone();
        let identity_account = account.clone();
        let generation = tokio::task::spawn_blocking(move || {
            identity_cache.result_cache_identity(&identity_root, &identity_account)
        })
        .await??;
        let key = format!("{generation}:{}", query.task_key());
        let request_token = request_token.unwrap_or_default().to_string();
        let spawn_sequence = {
            let mut inner = self.inner.lock().expect("ranking task cache poisoned");
            let should_start = !inner.tasks.contains_key(&key)
                || inner.tasks.get(&key).is_some_and(|task| {
                    matches!(
                        task.state,
                        RankingTaskState::Cancelled | RankingTaskState::Failed
                    ) && (request_token.is_empty() || task.request_token != request_token)
                });
            if should_start {
                for task in inner.tasks.values_mut() {
                    if matches!(
                        task.state,
                        RankingTaskState::Queued | RankingTaskState::Running
                    ) {
                        task.cancel.store(true, AtomicOrdering::Relaxed);
                        task.state = RankingTaskState::Cancelled;
                    }
                }
                inner.sequence += 1;
                let sequence = inner.sequence;
                inner.tasks.insert(
                    key.clone(),
                    RankingTask {
                        state: RankingTaskState::Queued,
                        processed: 0,
                        total: 0,
                        results: None,
                        error: None,
                        cancel: Arc::new(AtomicBool::new(false)),
                        sequence,
                        request_token: request_token.clone(),
                    },
                );
                trim_ranking_tasks(&mut inner);
                Some(sequence)
            } else {
                None
            }
        };
        if let Some(sequence) = spawn_sequence {
            let cache = self.clone();
            let task_key = key.clone();
            tokio::spawn(async move {
                cache
                    .run_task(root, account, task_key, sequence, query)
                    .await;
            });
        }
        self.snapshot(&key, cursor, limit)
    }

    async fn run_task(
        &self,
        root: PathBuf,
        account: String,
        key: String,
        sequence: u64,
        query: InsightsQuery,
    ) {
        let permit = match self.workers.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        {
            let mut inner = self.inner.lock().expect("ranking task cache poisoned");
            let Some(task) = inner.tasks.get_mut(&key) else {
                return;
            };
            if task.sequence != sequence {
                return;
            }
            if task.cancel.load(AtomicOrdering::Relaxed) {
                task.state = RankingTaskState::Cancelled;
                return;
            }
            task.state = RankingTaskState::Running;
        }
        if !self.same_cache.vector_index_available() {
            if let Some(task) = self
                .inner
                .lock()
                .expect("ranking task cache poisoned")
                .tasks
                .get_mut(&key)
                .filter(|task| task.sequence == sequence)
            {
                task.error = Some(
                    "此构建未启用 image-index-qdrant，无法保证完整的 SSCD ≥ 0.98 同图召回"
                        .to_string(),
                );
                task.state = RankingTaskState::Failed;
            }
            return;
        }
        while !self.same_cache.vector_index_ready() {
            if let Some(error) = self.same_cache.vector_index_error() {
                if let Some(task) = self
                    .inner
                    .lock()
                    .expect("ranking task cache poisoned")
                    .tasks
                    .get_mut(&key)
                    .filter(|task| task.sequence == sequence)
                {
                    task.error = Some(format!("SSCD 向量索引不可用：{error}"));
                    task.state = RankingTaskState::Failed;
                }
                return;
            }
            let cancelled = self
                .inner
                .lock()
                .expect("ranking task cache poisoned")
                .tasks
                .get(&key)
                .is_none_or(|task| {
                    task.sequence != sequence || task.cancel.load(AtomicOrdering::Relaxed)
                });
            if cancelled {
                if let Some(task) = self
                    .inner
                    .lock()
                    .expect("ranking task cache poisoned")
                    .tasks
                    .get_mut(&key)
                {
                    if task.sequence == sequence {
                        task.state = RankingTaskState::Cancelled;
                    }
                }
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let inner = self.inner.clone();
        let same_cache = self.same_cache.clone();
        let progress_key = key.clone();
        let progress_cancel = {
            let inner = self.inner.lock().expect("ranking task cache poisoned");
            let Some(task) = inner.tasks.get(&key) else {
                return;
            };
            if task.sequence != sequence {
                return;
            }
            task.cancel.clone()
        };
        let result = tokio::task::spawn_blocking(move || {
            compute_ranked_assets(
                &root,
                &account,
                &query,
                &same_cache,
                Some(progress_cancel.as_ref()),
                |processed, total| {
                    if let Some(task) = inner
                        .lock()
                        .expect("ranking task cache poisoned")
                        .tasks
                        .get_mut(&progress_key)
                        .filter(|task| task.sequence == sequence)
                    {
                        task.processed = task.processed.max(processed);
                        task.total = task.total.max(total);
                    }
                },
            )
        })
        .await;
        drop(permit);
        let mut inner = self.inner.lock().expect("ranking task cache poisoned");
        let Some(task) = inner.tasks.get_mut(&key) else {
            return;
        };
        if task.sequence != sequence {
            return;
        }
        if task.cancel.load(AtomicOrdering::Relaxed) {
            task.state = RankingTaskState::Cancelled;
            return;
        }
        match result {
            Ok(Ok(results)) => {
                task.processed = task.processed.max(task.total);
                task.results = Some(Arc::new(results));
                task.state = RankingTaskState::Complete;
            }
            Ok(Err(error)) => {
                task.error = Some(error.to_string());
                task.state = RankingTaskState::Failed;
            }
            Err(error) => {
                task.error = Some(error.to_string());
                task.state = RankingTaskState::Failed;
            }
        }
    }

    fn snapshot(&self, key: &str, cursor: Option<&str>, limit: usize) -> anyhow::Result<Value> {
        let inner = self.inner.lock().expect("ranking task cache poisoned");
        let task = inner
            .tasks
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("ranking task disappeared"))?;
        let progress = json!({
            "processed": task.processed,
            "total": task.total,
            "percent": if task.total == 0 { 0.0 } else { task.processed as f64 / task.total as f64 }
        });
        let status = match task.state {
            RankingTaskState::Complete => "complete",
            RankingTaskState::Failed => "failed",
            RankingTaskState::Cancelled => "cancelled",
            RankingTaskState::Queued | RankingTaskState::Running => "warming",
        };
        let Some(results) = &task.results else {
            return Ok(json!({
                "status": status,
                "ranking_scope": "candidate_pool",
                "candidate_limit": CANDIDATE_LIMIT,
                "exhaustive": false,
                "progress": progress,
                "items": [],
                "next_cursor": null,
                "error": task.error
            }));
        };
        let offset = decode_offset(cursor)?;
        let limit = limit.clamp(1, 100);
        let end = offset.saturating_add(limit).min(results.len());
        let items = if offset < results.len() {
            results[offset..end].to_vec()
        } else {
            Vec::new()
        };
        Ok(json!({
            "status": status,
            "ranking_scope": "candidate_pool",
            "candidate_limit": CANDIDATE_LIMIT,
            "exhaustive": false,
            "progress": progress,
            "items": items,
            "next_cursor": (end < results.len()).then(|| encode_offset(end)),
            "total": results.len(),
            "error": task.error
        }))
    }

    pub fn metrics_json(&self) -> Value {
        let inner = self.inner.lock().expect("ranking task cache poisoned");
        json!({
            "active": inner.tasks.values().filter(|task| task.state == RankingTaskState::Running).count(),
            "queued": inner.tasks.values().filter(|task| task.state == RankingTaskState::Queued).count(),
            "cached": inner.tasks.values().filter(|task| task.state == RankingTaskState::Complete).count(),
            "capacity": RANKING_CACHE_LIMIT
        })
    }
}

fn trim_ranking_tasks(inner: &mut RankingTaskInner) {
    while inner.tasks.len() > RANKING_CACHE_LIMIT {
        let removable = inner
            .tasks
            .iter()
            .filter(|(_, task)| {
                matches!(
                    task.state,
                    RankingTaskState::Complete
                        | RankingTaskState::Cancelled
                        | RankingTaskState::Failed
                )
            })
            .min_by_key(|(_, task)| task.sequence)
            .map(|(key, _)| key.clone());
        let Some(removable) = removable else {
            break;
        };
        inner.tasks.remove(&removable);
    }
}

fn encode_offset(offset: usize) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(offset.to_string())
}

fn decode_offset(cursor: Option<&str>) -> anyhow::Result<usize> {
    let Some(cursor) = cursor.filter(|value| !value.is_empty()) else {
        return Ok(0);
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cursor)?;
    Ok(String::from_utf8(bytes)?.parse()?)
}

pub fn maintenance_progress(root: &Path, account: &str) -> anyhow::Result<Value> {
    let con = open_manifest(root, account)?;
    let (tables, complete, current, total, facts, updated): (i64, i64, i64, i64, i64, String) = con
        .query_row(
            r#"select count(*),coalesce(sum(completed),0),
                      coalesce(sum(min(last_message_rowid,max_message_rowid)),0),
                      coalesce(sum(max_message_rowid),0),coalesce(sum(fact_count),0),
                      coalesce(max(updated_at),'')
               from image_popularity_analysis_state"#,
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
    let (build_schema, build_occurrences, build_facts, ready, build_updated) = con
        .query_row(
            r#"select schema_version,occurrence_count,fact_count,
                      summary_ready,refreshed_at
               from image_popularity_build_state where id=1"#,
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?
        .unwrap_or((0, 0, 0, false, String::new()));
    let legacy_facts_exist: bool = con.query_row(
        "select exists(select 1 from sqlite_master where type='table' and name='image_occurrence_message_facts')",
        [],
        |row| row.get(0),
    )?;
    let migration = con
        .query_row(
            r#"select processed_occurrences,
                      case when total_occurrences>0
                           then total_occurrences
                           else max_occurrence_rowid end,
                      completed,updated_at
               from image_occurrence_fact_migration_state where id=1"#,
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let migrating = legacy_facts_exist && !migration.as_ref().map(|state| state.2).unwrap_or(false);
    let (display_current, display_total, display_facts, display_updated, phase) = if migrating {
        let state = migration.unwrap_or((0, build_occurrences, false, build_updated.clone()));
        (
            state.0,
            state.1.max(build_occurrences),
            state.0.min(build_facts),
            state.3,
            "legacy_migration",
        )
    } else if ready && build_schema < 2 {
        (
            build_facts,
            build_occurrences,
            build_facts,
            build_updated,
            "complete",
        )
    } else {
        (
            current,
            total,
            facts,
            updated,
            if complete == tables && !ready {
                "aggregating"
            } else if ready {
                "complete"
            } else {
                "message_facts"
            },
        )
    };
    Ok(json!({
        "id": "image-popularity-analysis",
        "title": "热门图片消息事实与传播聚合",
        "status": if ready && !legacy_facts_exist && tables > 0 && complete == tables { "complete" } else if migrating || tables > 0 { "running" } else { "not_started" },
        "current": display_current,
        "total": display_total,
        "unit": "图片引用记录",
        "updated_at": display_updated,
        "details": {
            "tables_indexed": tables,
            "tables_incomplete": tables.saturating_sub(complete),
            "phase": phase,
            "rows_scanned": display_current,
            "occurrences_linked": display_facts,
            "occurrences_inspected": display_current,
            "occurrences_with_message_facts": display_facts
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "qq-insights-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn vector_blob(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn insert_asset(con: &Connection, id: i64, path: &str, width: i64, vector: &[f32]) {
        con.execute(
            r#"insert into image_assets(
                   id,path,file_size,width,height,sha256_hex,phash_hex,
                   quality_flags,stale
               ) values(?1,?2,100,?3,?3,?4,'abcd','',0)"#,
            params![id, path, width, format!("{id:064x}")],
        )
        .unwrap();
        con.execute(
            r#"insert into image_exact_groups(
                   asset_id,group_key,representative_asset_id,member_count,
                   total_bytes,duplicate_bytes
               ) values(?1,?2,?1,1,100,0)"#,
            params![id, format!("asset:{id}")],
        )
        .unwrap();
        con.execute(
            r#"insert into image_embeddings(
                   path,kind,model,dim,vec,sketch64_hex,bucket12,updated_at
               ) values(?1,'sscd','fixture',2,?2,'0000000000000001',7,'2026-01-01')"#,
            params![path, vector_blob(vector)],
        )
        .unwrap();
    }

    #[test]
    fn threshold_includes_exact_boundary() {
        assert!(0.9799_f64 + f64::EPSILON < SAME_IMAGE_THRESHOLD);
        assert!(!(0.98_f64 + f64::EPSILON < SAME_IMAGE_THRESHOLD));
    }

    #[test]
    fn message_date_uses_display_timezone_and_accepts_milliseconds() {
        let utc_seconds = 1_704_132_000_i64; // 2024-01-01 18:00:00 UTC
        let tokyo = chrono::FixedOffset::east_opt(9 * 60 * 60).unwrap();
        assert_eq!(
            message_date_in_timezone(utc_seconds, &chrono::Utc),
            "2024-01-01"
        );
        assert_eq!(message_date_in_timezone(utc_seconds, &tokyo), "2024-01-02");
        assert_eq!(
            message_date_in_timezone(utc_seconds * 1_000, &tokyo),
            "2024-01-02"
        );
    }

    #[test]
    fn query_normalizes_multiselects_for_stable_tasks() {
        let params = HashMap::from([
            ("senders".to_string(), "\"2\",1,2".to_string()),
            ("tables".to_string(), "group_2,group_1".to_string()),
        ]);
        let query = InsightsQuery::from_params(&params).unwrap();
        assert_eq!(query.senders, vec!["1", "2"]);
        assert_eq!(query.tables, vec!["group_1", "group_2"]);
        assert_eq!(query.task_key(), query.clone().task_key());
    }

    #[test]
    fn overview_key_ignores_ranking_only_options() {
        let popular = InsightsQuery {
            senders: vec!["1".to_string()],
            rank: "popular".to_string(),
            window: 30,
            ..InsightsQuery::default()
        };
        let growth = InsightsQuery {
            rank: "growth".to_string(),
            window: 180,
            ..popular.clone()
        };
        assert_ne!(popular.task_key(), growth.task_key());
        assert_eq!(popular.overview_key(), growth.overview_key());
    }

    #[test]
    fn overview_cache_evicts_only_the_least_recent_entry() {
        let mut cache = InsightsOverviewCache::default();
        for index in 0..OVERVIEW_CACHE_LIMIT {
            cache.insert(index.to_string(), json!(index));
        }
        assert_eq!(cache.get("0"), Some(json!(0)));
        cache.insert(
            OVERVIEW_CACHE_LIMIT.to_string(),
            json!(OVERVIEW_CACHE_LIMIT),
        );

        assert_eq!(cache.values.len(), OVERVIEW_CACHE_LIMIT);
        assert!(cache.values.contains_key("0"));
        assert!(!cache.values.contains_key("1"));
        assert!(cache.values.contains_key(&OVERVIEW_CACHE_LIMIT.to_string()));
    }

    #[test]
    fn filtered_overview_aggregates_in_sql_and_keeps_facets_linked() {
        let root = fixture_root("overview-filter");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let mut con = Connection::open(&manifest).unwrap();
        con.execute_batch(
            r#"
            insert into image_popularity_cube values
                (1,'2026-01-01','1','group_a','group',2),
                (2,'2026-01-01','2','group_a','group',3),
                (1,'2026-01-02','1','group_b','group',4),
                (3,'2026-01-02','1','group_a','group',5);
            "#,
        )
        .unwrap();
        let refreshed_at = current_timestamp();
        let tx = con.transaction().unwrap();
        insert_summary_scope(&tx, "all", "''", "1=1", &refreshed_at).unwrap();
        insert_summary_scope(
            &tx,
            "date",
            "message_date",
            "1=1 group by message_date",
            &refreshed_at,
        )
        .unwrap();
        insert_summary_scope(
            &tx,
            "sender",
            "sender_uin",
            "1=1 group by sender_uin",
            &refreshed_at,
        )
        .unwrap();
        insert_summary_scope(
            &tx,
            "conversation",
            "conversation_table",
            "1=1 group by conversation_table",
            &refreshed_at,
        )
        .unwrap();
        rebuild_daily_conversation_cache(&tx).unwrap();
        tx.execute(
            r#"insert into image_popularity_build_state(
                   id,schema_version,occurrence_count,fact_count,cube_row_count,
                   summary_ready,refreshed_at
               ) values(1,?1,0,0,4,1,?2)"#,
            params![POPULARITY_SCHEMA_VERSION, refreshed_at],
        )
        .unwrap();
        tx.commit().unwrap();
        drop(con);

        let query = InsightsQuery::from_params(&HashMap::from([(
            "senders".to_string(),
            "\"1\"".to_string(),
        )]))
        .unwrap();
        let value = overview(&root, account, &query).unwrap();
        assert_eq!(value["filters"]["senders"], json!(["1"]));
        assert_eq!(value["summary"]["references"], 11);
        assert_eq!(value["summary"]["images"], 2);
        assert_eq!(value["summary"]["senders"], 1);
        assert_eq!(value["summary"]["conversations"], 2);
        assert_eq!(value["timeline"][0]["count"], 5);
        assert_eq!(value["timeline"][0]["selected_count"], 2);
        assert_eq!(value["timeline"][1]["count"], 9);
        assert_eq!(value["timeline"][1]["selected_count"], 9);
        assert_eq!(value["senders"][0]["uin"], "1");
        assert_eq!(value["senders"][0]["count"], 11);
        assert_eq!(value["senders"][1]["uin"], "2");
        assert_eq!(value["senders"][1]["count"], 3);
        assert_eq!(value["conversations"][0]["table"], "group_a");
        assert_eq!(value["conversations"][0]["count"], 7);
        assert_eq!(value["conversations"][1]["table"], "group_b");
        assert_eq!(value["conversations"][1]["count"], 4);

        let table_query = InsightsQuery::from_params(&HashMap::from([
            ("tables".to_string(), "group_a".to_string()),
            ("from".to_string(), "2026-01-01".to_string()),
            ("to".to_string(), "2026-01-02".to_string()),
        ]))
        .unwrap();
        let table_value = overview(&root, account, &table_query).unwrap();
        assert_eq!(table_value["summary"]["references"], 10);
        assert_eq!(table_value["summary"]["images"], 3);
        assert_eq!(table_value["conversations"][0]["table"], "group_a");
        assert_eq!(table_value["conversations"][0]["count"], 10);
        assert_eq!(table_value["conversations"][1]["table"], "group_b");
        assert_eq!(table_value["conversations"][1]["count"], 4);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn completed_old_analysis_normalizes_local_dates_and_rebuilds_cube() {
        let root = fixture_root("popularity-local-date-upgrade");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let mut con = Connection::open(&manifest).unwrap();
        let timestamp = 1_704_132_000_i64;
        con.execute(
            r#"insert into image_assets(id,path,file_size,sha256_hex,stale)
               values(1,'one.png',100,'one',0)"#,
            [],
        )
        .unwrap();
        con.execute(
            r#"insert into image_exact_groups(
                   asset_id,group_key,representative_asset_id,member_count,
                   total_bytes,duplicate_bytes
               ) values(1,'asset:1',1,1,100,0)"#,
            [],
        )
        .unwrap();
        con.execute(
            r#"insert into image_asset_occurrences(
                   asset_id,conversation_table,message_rowid,message_time,
                   message_date,sender_uin,conversation_type,facts_updated_at
               ) values(1,'group_a',1,?1,'1999-01-01','sender','group','old')"#,
            [timestamp],
        )
        .unwrap();
        con.execute_batch(
            r#"insert into image_popularity_cube values
                   (1,'1999-01-01','sender','group_a','group',1);
            insert into image_popularity_build_state(
                id,schema_version,occurrence_count,fact_count,cube_row_count,
                summary_ready,refreshed_at
            ) values(1,3,1,1,1,1,'old');
            "#,
        )
        .unwrap();

        assert!(upgrade_completed_popularity_cache(&mut con).unwrap());
        let state = popularity_build_state(&con).unwrap().unwrap();
        assert_eq!(state.schema_version, POPULARITY_SCHEMA_VERSION);
        assert_eq!(state.occurrence_count, 1);
        assert_eq!(state.fact_count, 1);
        let expected_date = message_date(timestamp);
        let occurrence_date: String = con
            .query_row(
                "select message_date from image_asset_occurrences where asset_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let cube: (String, i64) = con
            .query_row(
                "select message_date,reference_count from image_popularity_cube",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(occurrence_date, expected_date);
        assert_eq!(cube, (expected_date, 1));
        assert!(!upgrade_completed_popularity_cache(&mut con).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn schema_never_contains_persistent_same_groups() {
        let root = fixture_root("schema");
        let manifest = image_index::default_manifest_path(&root, "1");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        let same_tables: i64 = con
            .query_row(
                "select count(*) from sqlite_master where type='table' and name='image_same_groups'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(same_tables, 0);
        assert!(con
            .query_row(
                "select 1 from sqlite_master where type='table' and name='image_popularity_cube'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .is_ok());
        let legacy_fact_tables: i64 = con
            .query_row(
                "select count(*) from sqlite_master where type='table' and name='image_occurrence_message_facts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_fact_tables, 0);
        let occurrence_columns = {
            let mut stmt = con
                .prepare("pragma table_info(image_asset_occurrences)")
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<HashSet<_>, _>>()
                .unwrap()
        };
        for column in [
            "message_time",
            "message_date",
            "sender_uin",
            "conversation_type",
            "facts_updated_at",
        ] {
            assert!(occurrence_columns.contains(column));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sscd_cache_honors_boundary_and_aliases_every_member() {
        let root = fixture_root("same-cache");
        let manifest = image_index::default_manifest_path(&root, "1");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        insert_asset(&con, 1, "one.png", 10, &[1.0, 0.0]);
        insert_asset(
            &con,
            2,
            "boundary.png",
            20,
            &[0.98, (1.0_f32 - 0.98_f32.powi(2)).sqrt()],
        );
        insert_asset(
            &con,
            3,
            "below.png",
            30,
            &[0.9799, (1.0_f32 - 0.9799_f32.powi(2)).sqrt()],
        );
        drop(con);

        let cache = SameImageCache::default();
        let group = cache.resolve(&root, "1", 1).unwrap();
        assert_eq!(group.exact_representative_ids, vec![1, 2]);
        assert_eq!(group.representative_id, 2);
        assert!(!group.exact_representative_ids.contains(&3));
        let alias = cache.resolve(&root, "1", 2).unwrap();
        assert!(Arc::ptr_eq(&group, &alias));
        assert_eq!(cache.misses.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(cache.hits.load(AtomicOrdering::Relaxed), 1);
        assert!(Arc::ptr_eq(&group, &cache.peek(1).unwrap()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn same_image_cache_peek_never_starts_cold_computation() {
        let cache = SameImageCache::default();
        assert!(cache.peek(42).is_none());
        assert_eq!(cache.misses.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(cache.metrics_json()["inflight"], 0);
    }

    #[test]
    fn concurrent_identical_same_image_queries_compute_once() {
        let root = fixture_root("same-concurrent");
        let manifest = image_index::default_manifest_path(&root, "1");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        insert_asset(&con, 1, "one.png", 10, &[1.0, 0.0]);
        drop(con);
        let cache = Arc::new(SameImageCache::default());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = cache.clone();
                let root = root.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    assert_eq!(cache.resolve(&root, "1", 1).unwrap().representative_id, 1);
                });
            }
        });
        assert_eq!(cache.misses.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(
            cache.hits.load(AtomicOrdering::Relaxed) + cache.misses.load(AtomicOrdering::Relaxed),
            8
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_or_model_identity_change_invalidates_whole_same_image_cache() {
        let root = fixture_root("same-invalidation");
        let manifest = image_index::default_manifest_path(&root, "1");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        insert_asset(&con, 1, "one.png", 10, &[1.0, 0.0]);
        drop(con);
        let cache = SameImageCache::default();
        let first = cache.resolve(&root, "1", 1).unwrap();

        let con = Connection::open(&manifest).unwrap();
        insert_asset(&con, 2, "two.png", 20, &[1.0, 0.0]);
        drop(con);
        cache.inner.lock().unwrap().identity_checked_at = None;
        let refreshed = cache.resolve(&root, "1", 1).unwrap();
        assert!(!Arc::ptr_eq(&first, &refreshed));
        assert_eq!(refreshed.exact_representative_ids, vec![1, 2]);
        assert_eq!(cache.invalidations.load(AtomicOrdering::Relaxed), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_identity_changes_when_existing_sscd_vector_is_recomputed() {
        let root = fixture_root("same-vector-generation");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let mut con = Connection::open(&manifest).unwrap();
        insert_asset(&con, 1, "one.png", 10, &[1.0, 0.0]);
        insert_asset(&con, 2, "two.png", 20, &[0.0, 1.0]);
        image_index::refresh_overview_cache(&mut con).unwrap();
        drop(con);
        let first = manifest_identity(&manifest, account).unwrap();

        let con = Connection::open(&manifest).unwrap();
        con.execute(
            r#"update image_embeddings
               set vec=?1,updated_at='2026-02-01'
               where path='one.png' and kind='sscd'"#,
            [vector_blob(&[0.5, 0.5])],
        )
        .unwrap();
        image_index::mark_image_index_content_changed(&con).unwrap();
        drop(con);
        let second = manifest_identity(&manifest, account).unwrap();

        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn weighted_lru_evicts_oldest_group_and_member_aliases() {
        let cache = SameImageCache::default();
        let mut inner = cache.inner.lock().unwrap();
        inner.identity = "fixture".to_string();
        for id in 1..=(SAME_CACHE_MAX_GROUPS as i64 + 1) {
            insert_same_group(
                &mut inner,
                Arc::new(SameImageGroup {
                    representative_id: id,
                    exact_representative_ids: vec![id],
                    asset_ids: vec![id],
                    scores: BTreeMap::from([(id, 1.0)]),
                }),
                &cache.evictions,
            );
        }
        assert_eq!(inner.entries.len(), SAME_CACHE_MAX_GROUPS);
        assert!(!inner.aliases.contains_key(&1));
        assert!(inner
            .aliases
            .contains_key(&(SAME_CACHE_MAX_GROUPS as i64 + 1)));
        assert_eq!(cache.evictions.load(AtomicOrdering::Relaxed), 1);
    }

    #[test]
    fn analysis_backfills_only_linked_messages_and_deduplicates_exact_group_per_message() {
        let root = fixture_root("facts");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        for (id, path) in [(1, "one.png"), (2, "copy.png")] {
            con.execute(
                "insert into image_assets(id,path,sha256_hex,stale) values(?1,?2,'same',0)",
                params![id, path],
            )
            .unwrap();
            con.execute(
                r#"insert into image_exact_groups(
                       asset_id,group_key,representative_asset_id,member_count,total_bytes,duplicate_bytes
                   ) values(?1,'sha256:same',1,2,2,1)"#,
                [id],
            )
            .unwrap();
        }
        for (asset, rowid) in [(1, 1), (2, 1), (1, 2)] {
            con.execute(
                r#"insert into image_asset_occurrences(
                       asset_id,conversation_table,message_rowid,linked_at
                   ) values(?1,'group_9',?2,'now')"#,
                params![asset, rowid],
            )
            .unwrap();
        }
        drop(con);
        let chat_db = root.join("chat.sqlite");
        let chat = Connection::open(&chat_db).unwrap();
        chat.execute(
            "create table group_9(Time integer,SenderUin integer,MsgContent blob,Info blob)",
            [],
        )
        .unwrap();
        chat.execute("insert into group_9 values(1704067200,100,x'',x'')", [])
            .unwrap();
        chat.execute("insert into group_9 values(1704153600,200,x'',x'')", [])
            .unwrap();
        chat.execute("insert into group_9 values(1704240000,300,x'',x'')", [])
            .unwrap();
        drop(chat);

        let report = build_popularity_analysis(&root, account, &chat_db, None).unwrap();
        assert!(report.cube_rebuilt);
        assert_eq!(report.occurrences, 3);
        assert_eq!(report.facts, 3);
        let con = Connection::open(&manifest).unwrap();
        let merged_facts: (i64, i64) = con
            .query_row(
                r#"select count(*),count(message_time)
                   from image_asset_occurrences
                   where facts_updated_at<>''"#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(merged_facts, (3, 3));
        let references: i64 = con
            .query_row(
                "select sum(reference_count) from image_popularity_cube",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(references, 2);
        let dates: i64 = con
            .query_row(
                "select count(*) from image_popularity_summary_cache where scope='date'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dates, 2);
        let refreshed_at: String = con
            .query_row(
                "select refreshed_at from image_popularity_build_state where id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(con);
        let repeated = build_popularity_analysis(&root, account, &chat_db, None).unwrap();
        assert!(!repeated.cube_rebuilt);
        assert_eq!(repeated.facts, 3);
        let con = Connection::open(&manifest).unwrap();
        let repeated_refreshed_at: String = con
            .query_row(
                "select refreshed_at from image_popularity_build_state where id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(repeated_refreshed_at, refreshed_at);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_fact_rows_migrate_into_occurrences_without_rereading_chat() {
        let root = fixture_root("legacy-facts");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        insert_asset(&con, 1, "one.png", 10, &[1.0, 0.0]);
        con.execute(
            r#"insert into image_asset_occurrences(
                   asset_id,conversation_table,message_rowid,linked_at
               ) values(1,'group_9',7,'old')"#,
            [],
        )
        .unwrap();
        con.execute_batch(
            r#"create table image_occurrence_message_facts (
                   asset_id integer not null,
                   conversation_table text not null,
                   message_rowid integer not null,
                   message_time integer not null,
                   message_date text not null,
                   sender_uin text not null,
                   conversation_type text not null,
                   updated_at text not null default '',
                   primary key(asset_id,conversation_table,message_rowid)
               );
               insert into image_occurrence_message_facts values(
                   1,'group_9',7,1704067200,'2024-01-01','88','group','legacy'
               );
               insert into image_occurrence_fact_migration_state(
                   id,last_occurrence_rowid,max_occurrence_rowid,completed,updated_at,scan_mode
               ) values(1,1,1,1,'old','occurrence_rowid_v1');"#,
        )
        .unwrap();
        drop(con);

        let chat_db = root.join("chat.sqlite");
        let chat = Connection::open(&chat_db).unwrap();
        chat.execute(
            "create table group_9(Time integer,SenderUin integer,MsgContent blob,Info blob)",
            [],
        )
        .unwrap();
        drop(chat);

        let report = build_popularity_analysis(&root, account, &chat_db, None).unwrap();
        assert_eq!(report.facts, 1);
        let con = Connection::open(&manifest).unwrap();
        let merged: (i64, String, String, String) = con
            .query_row(
                r#"select message_time,message_date,sender_uin,facts_updated_at
                   from image_asset_occurrences where asset_id=1"#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            merged,
            (
                1704067200,
                "2024-01-01".to_string(),
                "88".to_string(),
                "legacy".to_string()
            )
        );
        let references: i64 = con
            .query_row(
                "select coalesce(sum(reference_count),0) from image_popularity_cube",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let legacy_table_exists: bool = con
            .query_row(
                "select exists(select 1 from sqlite_master where type='table' and name='image_occurrence_message_facts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!legacy_table_exists);
        assert_eq!(references, 1);
        let migration_state_rows: i64 = con
            .query_row(
                "select count(*) from image_occurrence_fact_migration_state",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migration_state_rows, 0);
        drop(con);
        cleanup_superseded_occurrence_tables(&root, account).unwrap();
        let con = Connection::open(&manifest).unwrap();
        let superseded_tables: i64 = con
            .query_row(
                r#"select count(*) from sqlite_master
                   where type='table' and name in (
                       'image_asset_occurrences_v2_legacy',
                       'image_asset_occurrences_v3_migration'
                   )"#,
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(superseded_tables, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn maintenance_progress_does_not_hide_pending_legacy_migration() {
        let root = fixture_root("legacy-progress");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        con.execute_batch(
            r#"create table image_occurrence_message_facts (
                   asset_id integer not null,
                   conversation_table text not null,
                   message_rowid integer not null,
                   message_time integer not null,
                   message_date text not null,
                   sender_uin text not null,
                   conversation_type text not null,
                   updated_at text not null default '',
                   primary key(asset_id,conversation_table,message_rowid)
               );
               insert into image_popularity_analysis_state(
                   conversation_table,last_message_rowid,max_message_rowid,
                   occurrence_count,fact_count,completed,updated_at
               ) values('group_1',10,10,10,10,1,'analysis');
               insert into image_popularity_build_state(
                   id,schema_version,occurrence_count,fact_count,cube_row_count,
                   summary_ready,refreshed_at
               ) values(1,2,10,10,1,1,'build');
               insert into image_occurrence_fact_migration_state(
                   id,last_occurrence_rowid,max_occurrence_rowid,completed,
                   updated_at,scan_mode,processed_occurrences,total_occurrences
               ) values(
                   1,3,10,0,'migration','target_occurrence_rowid_v3',3,10
               );"#,
        )
        .unwrap();
        drop(con);

        let progress = maintenance_progress(&root, account).unwrap();
        assert_eq!(progress["status"], "running");
        assert_eq!(progress["current"], 3);
        assert_eq!(progress["total"], 10);
        assert_eq!(progress["details"]["phase"], "legacy_migration");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn candidate_pool_is_bounded_to_two_thousand_with_stable_order() {
        let root = fixture_root("candidate-pool");
        let manifest = image_index::default_manifest_path(&root, "1");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let mut con = Connection::open(&manifest).unwrap();
        let tx = con.transaction().unwrap();
        for id in 1..=2_001_i64 {
            tx.execute(
                r#"insert into image_popularity_cube(
                       representative_asset_id,message_date,sender_uin,
                       conversation_table,conversation_type,reference_count
                   ) values(?1,'2026-01-01','1','group_1','group',?2)"#,
                params![id, id],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let candidates =
            candidate_representatives(&con, &InsightsQuery::default(), None, None).unwrap();
        assert_eq!(candidates.len(), CANDIDATE_LIMIT);
        assert_eq!(candidates[0], 2_001);
        assert_eq!(candidates[CANDIDATE_LIMIT - 1], 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ranking_modes_apply_growth_new_and_revival_rules() {
        let group = SameImageGroup {
            representative_id: 1,
            exact_representative_ids: vec![1],
            asset_ids: vec![1],
            scores: BTreeMap::from([(1, 1.0)]),
        };
        let rows = vec![
            CubeRow {
                representative_id: 1,
                date: "2025-12-01".to_string(),
                sender: "1".to_string(),
                table: "group_1".to_string(),
                conversation_type: "group".to_string(),
                count: 2,
            },
            CubeRow {
                representative_id: 1,
                date: "2026-04-01".to_string(),
                sender: "2".to_string(),
                table: "group_2".to_string(),
                conversation_type: "group".to_string(),
                count: 5,
            },
        ];
        for rank in ["popular", "reach", "growth"] {
            let query = InsightsQuery {
                rank: rank.to_string(),
                ..InsightsQuery::default()
            };
            assert!(rank_group(
                &group,
                &rows,
                &query,
                Some("2026-04-01"),
                Some("2026-04-30"),
                Some("2026-03-02"),
                Some("2026-03-31"),
                None,
                None
            )
            .is_some());
        }
        let revival = InsightsQuery {
            rank: "revival".to_string(),
            ..InsightsQuery::default()
        };
        assert!(rank_group(
            &group,
            &rows,
            &revival,
            Some("2026-04-01"),
            Some("2026-04-30"),
            Some("2026-03-02"),
            Some("2026-03-31"),
            None,
            None
        )
        .is_some());
        let new = InsightsQuery {
            rank: "new".to_string(),
            ..InsightsQuery::default()
        };
        assert!(rank_group(
            &group,
            &rows,
            &new,
            Some("2026-04-01"),
            Some("2026-04-30"),
            None,
            None,
            None,
            None
        )
        .is_none());
    }

    #[test]
    fn filtered_ranking_limits_detail_rows_but_preserves_filtered_history() {
        let root = fixture_root("ranking-filtered-history");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        con.execute(
            "insert into image_assets(id,path,sha256_hex,stale) values(1,'1.png','same',0)",
            [],
        )
        .unwrap();
        con.execute(
            r#"insert into image_exact_groups(
                   asset_id,group_key,representative_asset_id,member_count,total_bytes,duplicate_bytes
               ) values(1,'sha256:same',1,1,1,0)"#,
            [],
        )
        .unwrap();
        con.execute_batch(
            r#"
            insert into image_popularity_cube values
                (1,'2020-01-01','9','group_other','group',10),
                (1,'2024-01-01','1','group_1','group',1),
                (1,'2025-12-15','1','group_1','group',2),
                (1,'2026-01-15','1','group_1','group',5);
            "#,
        )
        .unwrap();
        drop(con);
        let query = InsightsQuery {
            from: Some("2026-01-01".to_string()),
            to: Some("2026-01-31".to_string()),
            tables: vec!["group_1".to_string()],
            rank: "popular".to_string(),
            window: 90,
            ..InsightsQuery::default()
        };
        let ranked = compute_ranked_assets(
            &root,
            account,
            &query,
            &SameImageCache::default(),
            None,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].reference_count, 5);
        assert_eq!(ranked[0].growth, 3);
        assert_eq!(ranked[0].first_date, "2024-01-01");
        assert_eq!(
            ranked[0].trend,
            vec![json!({"date": "2026-01-15", "count": 5})]
        );

        let new_ranked = compute_ranked_assets(
            &root,
            account,
            &InsightsQuery {
                rank: "new".to_string(),
                ..query
            },
            &SameImageCache::default(),
            None,
            |_, _| {},
        )
        .unwrap();
        assert!(new_ranked.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn multi_value_filters_are_or_within_dimension_and_and_across_dimensions() {
        let query = InsightsQuery {
            from: Some("2026-01-01".to_string()),
            to: Some("2026-01-31".to_string()),
            senders: vec!["1".to_string(), "2".to_string()],
            tables: vec!["group_9".to_string()],
            conversation_types: vec!["group".to_string()],
            rank: "popular".to_string(),
            window: 90,
        };
        let row = |date: &str, sender: &str, table: &str| CubeRow {
            representative_id: 1,
            date: date.to_string(),
            sender: sender.to_string(),
            table: table.to_string(),
            conversation_type: "group".to_string(),
            count: 1,
        };
        assert!(matches_filters(
            &row("2026-01-10", "2", "group_9"),
            &query,
            true,
            true
        ));
        assert!(!matches_filters(
            &row("2026-01-10", "3", "group_9"),
            &query,
            true,
            true
        ));
        assert!(!matches_filters(
            &row("2026-01-10", "2", "group_8"),
            &query,
            true,
            true
        ));
        assert!(!matches_filters(
            &row("2026-02-01", "2", "group_9"),
            &query,
            true,
            true
        ));
    }

    #[tokio::test]
    async fn cold_ranking_progress_is_monotonic_and_completed_pages_are_stable() {
        let root = fixture_root("ranking-task");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let con = Connection::open(&manifest).unwrap();
        for id in 1..=2_i64 {
            con.execute(
                "insert into image_assets(id,path,sha256_hex,stale) values(?1,?2,?3,0)",
                params![id, format!("{id}.png"), format!("{id:064x}")],
            )
            .unwrap();
            con.execute(
                r#"insert into image_exact_groups(
                       asset_id,group_key,representative_asset_id,member_count,
                       total_bytes,duplicate_bytes
                   ) values(?1,?2,?1,1,1,0)"#,
                params![id, format!("asset:{id}")],
            )
            .unwrap();
            con.execute(
                r#"insert into image_popularity_cube(
                       representative_asset_id,message_date,sender_uin,
                       conversation_table,conversation_type,reference_count
                   ) values(?1,'2026-01-01','1','group_1','group',?2)"#,
                params![id, 3 - id],
            )
            .unwrap();
        }
        drop(con);
        let cache = RankingTaskCache::new(Arc::new(SameImageCache::default()));
        let query = InsightsQuery {
            rank: "popular".to_string(),
            window: 90,
            ..InsightsQuery::default()
        };
        let first = cache
            .request(
                root.clone(),
                account.to_string(),
                query.clone(),
                None,
                1,
                None,
            )
            .await
            .unwrap();
        assert_eq!(first["status"], "warming");
        let mut previous = 0;
        let mut completed = None;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let value = cache
                .request(
                    root.clone(),
                    account.to_string(),
                    query.clone(),
                    None,
                    1,
                    None,
                )
                .await
                .unwrap();
            let processed = value["progress"]["processed"].as_u64().unwrap() as usize;
            assert!(processed >= previous);
            previous = processed;
            if value["status"] == "complete" {
                completed = Some(value);
                break;
            }
            assert_ne!(value["status"], "failed", "{value}");
        }
        let complete = completed.expect("ranking task did not complete");
        assert_eq!(complete["items"][0]["id"], 1);
        let cursor = complete["next_cursor"].as_str().unwrap();
        let second = cache
            .request(
                root.clone(),
                account.to_string(),
                query.clone(),
                Some(cursor),
                1,
                None,
            )
            .await
            .unwrap();
        assert_eq!(second["items"][0]["id"], 2);
        let first_again = cache
            .request(
                root.clone(),
                account.to_string(),
                query.clone(),
                None,
                1,
                None,
            )
            .await
            .unwrap();
        assert_eq!(first_again["items"][0]["id"], 1);
        assert_eq!(
            first_again["progress"]["processed"],
            complete["progress"]["processed"]
        );

        let con = Connection::open(&manifest).unwrap();
        con.execute(
            "update image_popularity_cube set reference_count=case representative_asset_id when 1 then 1 else 9 end",
            [],
        )
        .unwrap();
        con.execute(
            r#"insert into image_popularity_build_state(
                   id,schema_version,occurrence_count,fact_count,cube_row_count,
                   summary_ready,refreshed_at
               ) values(1,3,10,10,2,1,'generation-two')"#,
            [],
        )
        .unwrap();
        drop(con);
        let generation_changed = cache
            .request(
                root.clone(),
                account.to_string(),
                query.clone(),
                None,
                1,
                None,
            )
            .await
            .unwrap();
        assert_eq!(generation_changed["status"], "warming");
        let mut refreshed = None;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let value = cache
                .request(
                    root.clone(),
                    account.to_string(),
                    query.clone(),
                    None,
                    1,
                    None,
                )
                .await
                .unwrap();
            if value["status"] == "complete" {
                refreshed = Some(value);
                break;
            }
        }
        assert_eq!(
            refreshed.expect("generation refresh did not complete")["items"][0]["id"],
            2
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn newest_ranking_request_cancels_running_work_and_cancelled_keys_restart() {
        let root = fixture_root("ranking-latest-wins");
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let cache = RankingTaskCache::new(Arc::new(SameImageCache::default()));
        let old_query = InsightsQuery {
            rank: "popular".to_string(),
            window: 90,
            ..InsightsQuery::default()
        };
        let generation = cache
            .same_cache
            .result_cache_identity(&root, account)
            .unwrap();
        let old_key = format!("{generation}:{}", old_query.task_key());
        let old_cancel = Arc::new(AtomicBool::new(false));
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.sequence = 1;
            inner.tasks.insert(
                old_key.clone(),
                RankingTask {
                    state: RankingTaskState::Running,
                    processed: 10,
                    total: 2_000,
                    results: None,
                    error: None,
                    cancel: old_cancel.clone(),
                    sequence: 1,
                    request_token: "old-token".to_string(),
                },
            );
        }
        let new_query = InsightsQuery {
            from: Some("2026-01-01".to_string()),
            to: Some("2026-01-31".to_string()),
            ..old_query.clone()
        };
        let new_key = format!("{generation}:{}", new_query.task_key());
        let value = cache
            .request(
                root.clone(),
                account.to_string(),
                new_query,
                None,
                10,
                Some("new-token"),
            )
            .await
            .unwrap();
        assert_eq!(value["status"], "warming");
        {
            let inner = cache.inner.lock().unwrap();
            let old = inner.tasks.get(&old_key).unwrap();
            assert_eq!(old.state, RankingTaskState::Cancelled);
            assert!(old_cancel.load(AtomicOrdering::Relaxed));
            assert!(!inner
                .tasks
                .get(&new_key)
                .unwrap()
                .cancel
                .load(AtomicOrdering::Relaxed));
        }

        let stale = cache
            .request(
                root.clone(),
                account.to_string(),
                old_query.clone(),
                None,
                10,
                Some("old-token"),
            )
            .await
            .unwrap();
        assert_eq!(stale["status"], "cancelled");
        assert!(!cache
            .inner
            .lock()
            .unwrap()
            .tasks
            .get(&new_key)
            .unwrap()
            .cancel
            .load(AtomicOrdering::Relaxed));

        let restarted = cache
            .request(
                root.clone(),
                account.to_string(),
                old_query,
                None,
                10,
                Some("returned-token"),
            )
            .await
            .unwrap();
        assert_eq!(restarted["status"], "warming");
        {
            let inner = cache.inner.lock().unwrap();
            let old = inner.tasks.get(&old_key).unwrap();
            assert!(old.sequence > 1);
            assert!(!old.cancel.load(AtomicOrdering::Relaxed));
            let new = inner.tasks.get(&new_key).unwrap();
            assert!(
                new.cancel.load(AtomicOrdering::Relaxed)
                    || matches!(
                        new.state,
                        RankingTaskState::Complete
                            | RankingTaskState::Cancelled
                            | RankingTaskState::Failed
                    )
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}

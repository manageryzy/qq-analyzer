use std::collections::HashMap;
#[cfg(feature = "image-index")]
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(feature = "image-index")]
use serde_json::Value;

use crate::info_storage;
use crate::msg3_log_service_avatar::resolve_group_avatar;
use crate::msg3_log_service_config::Config;
use crate::msg3_log_service_http::QueryParams;
use crate::msg3_log_service_info::info_summary;
use crate::msg3_log_service_models::{Conversation, ConversationLight};
use crate::msg3_log_service_tables::{quote_ident, split_table, table_names};
use crate::msg3_log_service_text::{first_nonempty, normalize_sender_show_name, usable_name};
use crate::msg3_log_service_time::{conversation_last_time, iso_time};
use crate::msg3_parser as parser;

const CONVERSATION_CACHE_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
struct ConversationCacheFile {
    version: u32,
    source_path: String,
    source_size: u64,
    source_mtime: u64,
    items: Vec<ConversationLight>,
}

fn source_signature(path: &Path) -> anyhow::Result<(String, u64, u64)> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let metadata = canonical.metadata()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs())
        .unwrap_or(0);
    Ok((canonical.display().to_string(), metadata.len(), modified))
}

/// Loads the lightweight conversation inventory from generated output, or
/// rebuilds it once from the prepared read-only chat database. Labels and
/// avatars remain on-demand so visible rows can be enriched without blocking
/// the complete inventory scan.
pub(crate) fn preload_conversation_cache(
    db_path: &Path,
    cache_path: &Path,
) -> anyhow::Result<HashMap<String, ConversationLight>> {
    let (source_path, source_size, source_mtime) = source_signature(db_path)?;
    if let Ok(bytes) = fs::read(cache_path) {
        if let Ok(cache) = serde_json::from_slice::<ConversationCacheFile>(&bytes) {
            if cache.version == CONVERSATION_CACHE_VERSION
                && cache.source_path == source_path
                && cache.source_size == source_size
                && cache.source_mtime == source_mtime
            {
                return Ok(cache
                    .items
                    .into_iter()
                    .map(|item| (item.table.clone(), item))
                    .collect());
            }
        }
    }

    let con = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut items = Vec::new();
    for table in table_names(&con)? {
        items.push(
            conversation_light_fast(&con, &table)
                .unwrap_or_else(|_| fallback_conversation_light(&table)),
        );
    }
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        cache_path,
        serde_json::to_vec(&ConversationCacheFile {
            version: CONVERSATION_CACHE_VERSION,
            source_path,
            source_size,
            source_mtime,
            items: items.clone(),
        })?,
    )?;
    Ok(items
        .into_iter()
        .map(|item| (item.table.clone(), item))
        .collect())
}

pub(crate) fn conversation_meta(
    root: &Path,
    account: &str,
    con: &Connection,
    info: &info_storage::InfoStorage,
    table: &str,
) -> anyhow::Result<Conversation> {
    let (conv_type, id) = split_table(table);
    let quoted = quote_ident(table);
    let last = conversation_last_time(con, &quoted)?;
    let (label, source) = conversation_label(account, con, info, table, &conv_type, &id)?;
    let (group_memo, group_avatar_url, group_avatar_candidates, group_avatar_reason, group_profile) =
        if conv_type == "group" {
            let profile = info.group_profile(&id)?;
            let (url, candidates, reason) = resolve_group_avatar(root, account, &profile);
            (
                first_nonempty(&[&profile.memo, &profile.finger_memo]),
                url,
                candidates,
                reason,
                serde_json::to_value(&profile).unwrap_or_else(|_| json!({})),
            )
        } else {
            (
                String::new(),
                String::new(),
                Vec::new(),
                String::new(),
                json!({}),
            )
        };
    Ok(Conversation {
        table: table.to_string(),
        key: table.to_string(),
        conv_type,
        id,
        rows: last.rows,
        last_time: last.last_time,
        last_datetime: iso_time(last.last_time),
        label,
        raw_last_time: last.raw_last_time,
        raw_last_datetime: iso_time(last.raw_last_time),
        time_anomaly_count: last.time_anomaly_count,
        label_source: source,
        group_memo,
        group_avatar_url,
        group_avatar_candidates,
        group_avatar_reason,
        group_profile,
    })
}

fn conversation_light_fast(con: &Connection, table: &str) -> anyhow::Result<ConversationLight> {
    let (conv_type, id) = split_table(table);
    let quoted = quote_ident(table);
    let last = conversation_last_time(con, &quoted)?;
    let fallback = match conv_type.as_str() {
        "buddy" => "私聊".to_string(),
        "group" => "群聊".to_string(),
        "discuss" => "讨论组".to_string(),
        "system" => "系统".to_string(),
        _ => conv_type.clone(),
    };
    Ok(ConversationLight {
        table: table.to_string(),
        key: table.to_string(),
        conv_type,
        id: id.clone(),
        rows: last.rows,
        rows_is_estimate: true,
        last_time: last.last_time,
        last_datetime: iso_time(last.last_time),
        label: format!("{fallback} {id}"),
        label_source_basic: "pending".to_string(),
        group_avatar_url: String::new(),
        group_avatar_candidates: Vec::new(),
        group_avatar_reason: String::new(),
    })
}

fn resolve_conversation_light_metadata(
    root: &Path,
    account: &str,
    con: &Connection,
    info: &info_storage::InfoStorage,
    light_cache: &mut HashMap<String, ConversationLight>,
    mut meta: ConversationLight,
) -> ConversationLight {
    if meta.label_source_basic == "pending" {
        if let Ok((label, source)) =
            conversation_label_light(account, con, info, &meta.table, &meta.conv_type, &meta.id)
        {
            meta.label = label;
            meta.label_source_basic = source;
        } else {
            meta.label_source_basic = "fallback".to_string();
        }
    }
    if meta.conv_type == "group" && meta.group_avatar_reason.is_empty() {
        if let Ok(profile) = info.group_profile(&meta.id) {
            let (url, candidates, reason) = resolve_group_avatar(root, account, &profile);
            meta.group_avatar_url = url;
            meta.group_avatar_candidates = candidates;
            meta.group_avatar_reason = reason;
        }
    }
    light_cache.insert(meta.table.clone(), meta.clone());
    meta
}

fn fallback_conversation_light(table: &str) -> ConversationLight {
    let (conv_type, id) = split_table(table);
    ConversationLight {
        table: table.to_string(),
        key: table.to_string(),
        conv_type: conv_type.clone(),
        id: id.clone(),
        rows: 0,
        rows_is_estimate: true,
        last_time: 0,
        last_datetime: String::new(),
        label: format!("{} {}", conv_type, id),
        label_source_basic: "fallback".to_string(),
        group_avatar_url: String::new(),
        group_avatar_candidates: Vec::new(),
        group_avatar_reason: String::new(),
    }
}

pub(crate) fn conversations_json(
    root: &Path,
    account: &str,
    con: &Connection,
    info: &info_storage::InfoStorage,
    light_cache: &mut HashMap<String, ConversationLight>,
    q: &QueryParams,
) -> anyhow::Result<String> {
    let conv_type_filter = q.get("type").map(String::as_str).unwrap_or("");
    let needle = q.get("q").map(|s| s.to_lowercase()).unwrap_or_default();
    let offset = q
        .get("offset")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(80)
        .min(200);
    let mut items = Vec::new();
    for table in table_names(con)? {
        let (conv_type, _) = split_table(&table);
        if !conv_type_filter.is_empty() && conv_type != conv_type_filter {
            continue;
        }
        let mut meta = if let Some(cached) = light_cache.get(&table) {
            cached.clone()
        } else {
            let meta = conversation_light_fast(con, &table)
                .unwrap_or_else(|_| fallback_conversation_light(&table));
            light_cache.insert(table.clone(), meta.clone());
            meta
        };
        if !needle.is_empty() && meta.label_source_basic == "pending" {
            meta = resolve_conversation_light_metadata(root, account, con, info, light_cache, meta);
        }
        let hay = format!("{} {} {} {}", meta.table, meta.key, meta.id, meta.label).to_lowercase();
        if !needle.is_empty() && !hay.contains(&needle) {
            continue;
        }
        items.push(meta);
    }
    items.sort_by(|a, b| {
        b.last_time
            .cmp(&a.last_time)
            .then_with(|| a.table.cmp(&b.table))
    });
    let total = items.len();
    let page: Vec<_> = items.into_iter().skip(offset).take(limit).collect();
    Ok(json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "counts": {},
        "items": page
    })
    .to_string())
}

pub(crate) fn conversation_detail_json(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    detail_cache: &mut HashMap<String, Conversation>,
    q: &QueryParams,
) -> anyhow::Result<String> {
    let table = q.get("table").cloned().unwrap_or_default();
    let detail = cached_conversation_detail(cfg, con, info, detail_cache, &table)?;
    Ok(serde_json::to_string(&detail)?)
}

pub(crate) fn conversation_details_json(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    detail_cache: &mut HashMap<String, Conversation>,
    q: &QueryParams,
) -> anyhow::Result<String> {
    let tables = q
        .get("tables")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .take(40)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut items = Vec::new();
    for table in tables {
        if let Ok(detail) = cached_conversation_detail(cfg, con, info, detail_cache, table) {
            items.push(detail);
        }
    }
    Ok(json!({ "items": items }).to_string())
}

pub(crate) fn cached_conversation_detail(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    detail_cache: &mut HashMap<String, Conversation>,
    table: &str,
) -> anyhow::Result<Conversation> {
    if let Some(cached) = detail_cache.get(table) {
        return Ok(cached.clone());
    }
    let detail = conversation_meta(&cfg.root, &cfg.account, con, info, table)?;
    detail_cache.insert(table.to_string(), detail.clone());
    Ok(detail)
}

#[cfg(feature = "image-index")]
pub(crate) fn enrich_insights_overview_labels(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    detail_cache: &mut HashMap<String, Conversation>,
    overview: &mut Value,
) {
    let sender_ids = overview["senders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["uin"].as_str())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut sender_labels = info.labels("buddy", &sender_ids).unwrap_or_default();
    let mut unresolved = sender_ids
        .iter()
        .filter(|uin| !sender_labels.contains_key(*uin))
        .cloned()
        .collect::<HashSet<_>>();

    let conversation_keys = overview["conversations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some((
                item["table"].as_str()?.to_string(),
                item["type"].as_str()?.to_string(),
                item["id"].as_str()?.to_string(),
            ))
        })
        .collect::<Vec<_>>();

    // A sender may not be a direct contact but still has a group card. Check
    // the most popular visible groups in order and stop as soon as every
    // visible sender has a label.
    for (_, _, group_id) in conversation_keys
        .iter()
        .filter(|(_, kind, _)| kind == "group")
        .take(100)
    {
        if unresolved.is_empty() {
            break;
        }
        let profiles = info
            .group_member_profiles_for(group_id, &unresolved)
            .unwrap_or_default();
        for (uin, profile) in profiles {
            if usable_name(&profile.display_name, &uin, &cfg.account) {
                sender_labels.insert(uin.clone(), profile.display_name);
                unresolved.remove(&uin);
            }
        }
    }
    if let Some(senders) = overview["senders"].as_array_mut() {
        for item in senders {
            let Some(uin) = item["uin"].as_str() else {
                continue;
            };
            if let Some(label) = sender_labels.get(uin) {
                item["label"] = json!(label);
            } else if uin == cfg.account {
                item["label"] = json!("自己");
            }
        }
    }

    let mut wanted_by_kind = HashMap::<String, HashSet<String>>::new();
    for (_, kind, id) in &conversation_keys {
        wanted_by_kind
            .entry(kind.clone())
            .or_default()
            .insert(id.clone());
    }
    let mut conversation_labels = HashMap::<String, String>::new();
    for (kind, wanted) in wanted_by_kind {
        for (id, label) in info.labels(&kind, &wanted).unwrap_or_default() {
            conversation_labels.insert(format!("{kind}_{id}"), label);
        }
    }
    for (table, _, id) in &conversation_keys {
        if conversation_labels
            .get(table)
            .is_some_and(|label| usable_name(label, id, &cfg.account))
        {
            continue;
        }
        if let Ok(detail) = cached_conversation_detail(cfg, con, info, detail_cache, table) {
            if usable_name(&detail.label, id, &cfg.account) {
                conversation_labels.insert(table.clone(), detail.label);
            }
        }
    }
    if let Some(conversations) = overview["conversations"].as_array_mut() {
        for item in conversations {
            let Some(table) = item["table"].as_str() else {
                continue;
            };
            if let Some(label) = conversation_labels.get(table) {
                item["label"] = json!(label);
            }
        }
    }
}

fn conversation_label(
    account: &str,
    con: &Connection,
    info: &info_storage::InfoStorage,
    table: &str,
    conv_type: &str,
    id: &str,
) -> anyhow::Result<(String, String)> {
    if conv_type == "system" {
        return Ok((format!("系统 {id}"), "fallback".to_string()));
    }
    if conv_type == "group" {
        let rename = group_rename_label(con, table)?;
        if usable_name(&rename, id, account) {
            return Ok((rename, "system_rename".to_string()));
        }
    }
    let indexed = info.label(conv_type, id)?;
    if usable_name(&indexed, id, account) {
        return Ok((indexed, "infostorage".to_string()));
    }
    if conv_type == "group" || conv_type == "discuss" {
        let fallback = if conv_type == "group" {
            "群聊"
        } else {
            "讨论组"
        };
        return Ok((format!("{fallback} {id}"), "fallback".to_string()));
    }
    let quoted = quote_ident(table);
    let mut stmt = con.prepare(&format!(
        "select SenderUin, Info from {quoted} order by rowid asc limit 32"
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let sender: i64 = row.get(0)?;
        let info: Vec<u8> = row.get(1)?;
        let summary = info_summary(&info);
        let sender_name = normalize_sender_show_name(
            &summary
                .get("strSenderShowName")
                .cloned()
                .unwrap_or_default(),
            &sender.to_string(),
            account,
            id,
        );
        if sender.to_string() != account && usable_name(&sender_name, id, account) {
            return Ok((sender_name, "message_info_sender".to_string()));
        }
    }
    let fallback = match conv_type {
        "buddy" => "私聊",
        "group" => "群聊",
        "discuss" => "讨论组",
        _ => conv_type,
    };
    Ok((format!("{fallback} {id}"), "fallback".to_string()))
}

fn conversation_label_light(
    account: &str,
    con: &Connection,
    info: &info_storage::InfoStorage,
    table: &str,
    conv_type: &str,
    id: &str,
) -> anyhow::Result<(String, String)> {
    if conv_type == "system" {
        return Ok((format!("系统 {id}"), "fallback".to_string()));
    }
    let indexed = info.label(conv_type, id)?;
    if usable_name(&indexed, id, account) {
        return Ok((indexed, "infostorage".to_string()));
    }
    if conv_type == "group" || conv_type == "discuss" {
        let fallback = if conv_type == "group" {
            "群聊"
        } else {
            "讨论组"
        };
        return Ok((format!("{fallback} {id}"), "fallback".to_string()));
    }
    let quoted = quote_ident(table);
    let mut stmt = con.prepare(&format!(
        "select SenderUin, Info from {quoted} order by rowid asc limit 8"
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let sender: i64 = row.get(0)?;
        let info: Vec<u8> = row.get(1)?;
        let summary = info_summary(&info);
        let sender_name = normalize_sender_show_name(
            &summary
                .get("strSenderShowName")
                .cloned()
                .unwrap_or_default(),
            &sender.to_string(),
            account,
            id,
        );
        if sender.to_string() != account && usable_name(&sender_name, id, account) {
            return Ok((sender_name, "message_info_sender".to_string()));
        }
    }
    let fallback = match conv_type {
        "buddy" => "私聊",
        "group" => "群聊",
        "discuss" => "讨论组",
        _ => conv_type,
    };
    Ok((format!("{fallback} {id}"), "fallback".to_string()))
}

fn group_rename_label(con: &Connection, table: &str) -> anyhow::Result<String> {
    let quoted = quote_ident(table);
    for order in ["asc", "desc"] {
        let sql =
            format!("select SenderUin, MsgContent from {quoted} order by rowid {order} limit 256");
        let mut stmt = con.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let sender: i64 = row.get(0)?;
            if !matches!(sender, 0 | 10000 | 1000000) {
                continue;
            }
            let msg: Vec<u8> = row.get(1)?;
            let (text, _, _, _) = parser::parse_msgcontent_outputs(&msg);
            if let Some(label) = extract_group_rename(&text) {
                return Ok(label);
            }
        }
    }
    Ok(String::new())
}

fn extract_group_rename(text: &str) -> Option<String> {
    for marker in ["修改了群名称为“", "修改了群名称为\""] {
        let Some(start) = text.find(marker) else {
            continue;
        };
        let rest = &text[start + marker.len()..];
        let end = rest.find(['”', '"']).unwrap_or(rest.len());
        let label = rest[..end].trim();
        if !label.is_empty() && label.chars().count() <= 80 {
            return Some(label.to_string());
        }
    }
    None
}

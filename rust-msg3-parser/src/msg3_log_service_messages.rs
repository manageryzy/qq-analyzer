#[cfg(feature = "image-index")]
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::info_storage;
use crate::msg3_log_service_assets::{match_assets_with_resolver, AssetResolver};
use crate::msg3_log_service_avatar::resolve_member_avatar;
use crate::msg3_log_service_config::Config;
#[cfg(feature = "image-index")]
use crate::msg3_log_service_conversations::cached_conversation_detail;
use crate::msg3_log_service_conversations::conversation_meta;
use crate::msg3_log_service_http::QueryParams;
use crate::msg3_log_service_info::info_summary;
use crate::msg3_log_service_models::{Conversation, MessageItem};
use crate::msg3_log_service_rich::{
    annotate_quote_nodes, attach_mmp_items_to_multi_msg, message_style_meta,
    quote_refs_from_elements,
};
use crate::msg3_log_service_tables::quote_ident;
use crate::msg3_log_service_text::{
    first_nonempty, merge_member_label_with_message_info, normalize_sender_show_name,
};
use crate::msg3_log_service_time::iso_time;
use crate::msg3_parser as parser;

pub(crate) fn messages_json(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    friend_social: &info_storage::InfoStorage,
    q: &QueryParams,
) -> anyhow::Result<String> {
    let table = q.get("table").cloned().unwrap_or_default();
    let offset = q
        .get("offset")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let before = q
        .get("before")
        .map(|s| s == "1" || s == "true")
        .unwrap_or(false);
    let meta = conversation_meta(&cfg.root, &cfg.account, con, info, &table)?;
    let quoted = quote_ident(&table);
    let rowid_cursor = offset.max(1);
    let senders = page_senders_rowid(con, &quoted, limit, rowid_cursor, before)?;
    let members = if meta.conv_type == "group" {
        info.group_member_profiles_for(&meta.id, &senders)?
    } else {
        HashMap::new()
    };
    let contacts = merged_contact_profiles(info, friend_social, &senders)?;
    let total = meta.rows;
    let max_rowid: i64 = con
        .query_row(&format!("select max(rowid) from {quoted}"), [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0);
    let min_rowid: i64 = con
        .query_row(&format!("select min(rowid) from {quoted}"), [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0);
    let sql = if before {
        format!(
            "select rowid, Time, Rand, SenderUin, MsgContent, Info from \
             (select rowid, Time, Rand, SenderUin, MsgContent, Info from {quoted} where rowid < ? order by rowid desc limit ?) \
             order by rowid asc"
        )
    } else {
        format!("select rowid, Time, Rand, SenderUin, MsgContent, Info from {quoted} where rowid >= ? order by rowid asc limit ?")
    };
    let mut stmt = con.prepare(&sql)?;
    let mut rows = stmt.query(params![rowid_cursor, limit])?;
    let mut items = Vec::new();
    let mut first_rowid = 0;
    let mut last_rowid = 0;
    // FileIndex/Resume data can be large. Reuse one resolver for the complete
    // page so it is loaded at most once instead of once per message.
    let mut asset_resolver = AssetResolver::new(&cfg.root, &cfg.account);
    while let Some(row) = rows.next()? {
        let item = message_item(
            row,
            con,
            &quoted,
            &meta,
            &members,
            &contacts,
            &cfg.root,
            &cfg.account,
            &mut asset_resolver,
        )?;
        if first_rowid == 0 {
            first_rowid = item.rowid;
        }
        last_rowid = item.rowid;
        items.push(json!({
            "table": item.table,
            "rowid": item.rowid,
            "time": item.time,
            "datetime": item.datetime,
            "time_is_plausible": item.time_is_plausible,
            "rand": item.rand,
            "sender_uin": item.sender_uin,
            "is_self": item.is_self,
            "display_sender": item.display_sender,
            "display_sender_line": item.display_sender_line,
            "sender_identity_note": item.sender_identity_note,
            "avatar_url": item.avatar_url,
            "text": item.text,
            "display_text": item.display_text,
            "rich_nodes": item.rich_nodes,
            "style_meta": item.style_meta,
            "assets": item.assets,
            "media_kind": item.media_kind,
            "media_label": item.media_label,
            "candidate_path_count": item.candidate_path_count,
            "candidate_path_hit_count": item.candidate_path_hit_count,
            "unmatched_reason": item.unmatched_reason,
            "msgcontent_len": item.msgcontent_len,
            "info_len": item.info_len
        }));
    }
    Ok(json!({
        "table": table,
        "key": meta.key,
        "conversation": meta,
        "total": total,
        "max_rowid": max_rowid,
        "offset": offset,
        "limit": limit,
        "paging_mode": "rowid",
        "before": before,
        "first_rowid": first_rowid,
        "last_rowid": last_rowid,
        "prev_offset": first_rowid,
        "next_offset": last_rowid.saturating_add(1),
        "has_prev": first_rowid > min_rowid,
        "has_next": last_rowid > 0 && last_rowid < max_rowid,
        "items": items
    })
    .to_string())
}

pub(crate) fn message_detail_json(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    friend_social: &info_storage::InfoStorage,
    q: &QueryParams,
) -> anyhow::Result<String> {
    let table = q.get("table").cloned().unwrap_or_default();
    let rowid = q
        .get("rowid")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let meta = conversation_meta(&cfg.root, &cfg.account, con, info, &table)?;
    let quoted = quote_ident(&table);
    let senders = row_sender(con, &quoted, rowid)?.into_iter().collect();
    let members = if meta.conv_type == "group" {
        info.group_member_profiles_for(&meta.id, &senders)?
    } else {
        HashMap::new()
    };
    let contacts = merged_contact_profiles(info, friend_social, &senders)?;
    let mut asset_resolver = AssetResolver::new(&cfg.root, &cfg.account);
    let mut stmt = con.prepare(&format!(
        "select rowid, Time, Rand, SenderUin, MsgContent, Info from {quoted} where rowid = ?"
    ))?;
    let item = stmt.query_row(params![rowid], |row| {
        message_item(
            row,
            con,
            &quoted,
            &meta,
            &members,
            &contacts,
            &cfg.root,
            &cfg.account,
            &mut asset_resolver,
        )
        .map_err(|err| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                err.to_string(),
            )))
        })
    })?;
    Ok(serde_json::to_string(&item)?)
}

#[cfg(feature = "image-index")]
#[derive(Clone, Debug)]
struct ReferenceFact {
    table: String,
    rowid: i64,
    linked_at: String,
    time: i64,
    datetime: String,
    sender_uin: String,
    sender_label: String,
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "image-index")]
pub(crate) fn image_reference_analysis_json(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    friend_social: &info_storage::InfoStorage,
    detail_cache: &mut HashMap<String, Conversation>,
    asset_id: i64,
    same_image_asset_ids: Vec<i64>,
    references: Vec<(String, i64, String)>,
    scan: Value,
    q: &QueryParams,
) -> anyhow::Result<String> {
    let offset = q
        .get("offset")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = q
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(12)
        .clamp(1, 30);
    let radius = q
        .get("context")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(5)
        .clamp(0, 5);
    let sender_filter = q
        .get("sender")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let sender_filters = q
        .get("senders")
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<HashSet<_>>()
        })
        .filter(|values| !values.is_empty());
    let table_filter = q
        .get("table")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let table_filters = q
        .get("tables")
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<HashSet<_>>()
        })
        .filter(|values| !values.is_empty());
    let conversation_type_filter = q
        .get("conversation_type")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let period_filter = q
        .get("period")
        .map(String::as_str)
        .filter(|value| value.len() == 7 && value.as_bytes().get(4) == Some(&b'-'));
    let valid_date = |value: &&String| {
        value.len() == 10
            && value.as_bytes().get(4) == Some(&b'-')
            && value.as_bytes().get(7) == Some(&b'-')
    };
    let date_from_filter = q.get("date_from").filter(valid_date).map(String::as_str);
    let date_to_filter = q.get("date_to").filter(valid_date).map(String::as_str);

    let indexed_occurrences = references.len();
    let mut grouped: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    for (table, rowid, linked_at) in references {
        grouped.entry(table).or_default().push((rowid, linked_at));
    }
    let mut facts = Vec::with_capacity(indexed_occurrences);
    for (table, rows) in &grouped {
        let quoted = quote_ident(table);
        let (_, conversation_id) = crate::msg3_log_service_tables::split_table(table);
        let linked = rows
            .iter()
            .map(|(rowid, linked_at)| (*rowid, linked_at.as_str()))
            .collect::<HashMap<_, _>>();
        for chunk in rows.chunks(400) {
            let placeholders = (1..=chunk.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "select rowid,Time,SenderUin,Info from {quoted} where rowid in ({placeholders})"
            );
            let values = chunk
                .iter()
                .map(|(rowid, _)| rusqlite::types::Value::Integer(*rowid))
                .collect::<Vec<_>>();
            let mut stmt = match con.prepare(&sql) {
                Ok(stmt) => stmt,
                Err(_) => continue,
            };
            let mut result_rows = stmt.query(rusqlite::params_from_iter(values.iter()))?;
            while let Some(row) = result_rows.next()? {
                let rowid = row.get::<_, i64>(0)?;
                let time = row.get::<_, i64>(1)?;
                let sender = row.get::<_, i64>(2)?.to_string();
                let raw_info = row.get::<_, Vec<u8>>(3)?;
                let summary = info_summary(&raw_info);
                let sender_name = normalize_sender_show_name(
                    &summary
                        .get("strSenderShowName")
                        .cloned()
                        .unwrap_or_default(),
                    &sender,
                    &cfg.account,
                    &conversation_id,
                );
                let self_label = if sender == cfg.account {
                    "自己".to_string()
                } else {
                    String::new()
                };
                facts.push(ReferenceFact {
                    table: table.clone(),
                    rowid,
                    linked_at: linked.get(&rowid).copied().unwrap_or_default().to_string(),
                    time,
                    datetime: iso_time(time),
                    sender_label: first_nonempty(&[&sender_name, &self_label, &sender]),
                    sender_uin: sender,
                });
            }
        }
    }
    facts.sort_by(|left, right| {
        right
            .time
            .cmp(&left.time)
            .then_with(|| left.table.cmp(&right.table))
            .then_with(|| right.rowid.cmp(&left.rowid))
    });

    let mut senders: HashMap<String, (usize, HashMap<String, usize>, String, i64)> = HashMap::new();
    let mut conversations: HashMap<String, usize> = HashMap::new();
    let mut timeline: BTreeMap<String, usize> = BTreeMap::new();
    let mut daily_timeline: BTreeMap<String, usize> = BTreeMap::new();
    let mut sender_selected_counts: HashMap<String, usize> = HashMap::new();
    let mut conversation_selected_counts: HashMap<String, usize> = HashMap::new();
    let mut timeline_selected_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut daily_selected_counts: BTreeMap<String, usize> = BTreeMap::new();
    for fact in &facts {
        let entry = senders
            .entry(fact.sender_uin.clone())
            .or_insert_with(|| (0, HashMap::new(), fact.table.clone(), fact.rowid));
        entry.0 += 1;
        *entry.1.entry(fact.sender_label.clone()).or_default() += 1;
        *conversations.entry(fact.table.clone()).or_default() += 1;
        let period = fact
            .datetime
            .get(0..7)
            .filter(|value| value.as_bytes().get(4) == Some(&b'-'))
            .unwrap_or("未知时间")
            .to_string();
        *timeline.entry(period.clone()).or_default() += 1;
        let date = fact.datetime.get(0..10).unwrap_or("未知日期").to_string();
        *daily_timeline.entry(date.clone()).or_default() += 1;

        // Every facet uses the complete active filter set. This makes sender,
        // conversation, and month counts move together instead of leaving a
        // selected facet at its unfiltered totals.
        let selected = sender_filter.is_none_or(|sender| fact.sender_uin == sender)
            && sender_filters
                .as_ref()
                .is_none_or(|senders| senders.contains(fact.sender_uin.as_str()))
            && table_filter.is_none_or(|table| fact.table == table)
            && table_filters
                .as_ref()
                .is_none_or(|tables| tables.contains(fact.table.as_str()))
            && conversation_type_filter.is_none_or(|selected| {
                crate::msg3_log_service_tables::split_table(&fact.table).0 == selected
            })
            && period_filter.is_none_or(|selected| period == selected)
            && date_from_filter.is_none_or(|from| date.as_str() >= from)
            && date_to_filter.is_none_or(|to| date.as_str() <= to);
        if selected {
            *sender_selected_counts
                .entry(fact.sender_uin.clone())
                .or_default() += 1;
            *conversation_selected_counts
                .entry(fact.table.clone())
                .or_default() += 1;
            *timeline_selected_counts.entry(period).or_default() += 1;
            *daily_selected_counts.entry(date).or_default() += 1;
        }
    }

    let unique_senders = senders.len();
    let mut top_senders = senders
        .into_iter()
        .map(|(uin, (count, labels, latest_table, latest_rowid))| {
            let selected_count = sender_selected_counts.get(&uin).copied().unwrap_or(0);
            let label = labels
                .into_iter()
                .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
                .map(|value| value.0)
                .unwrap_or_else(|| uin.clone());
            json!({
                "uin": uin,
                "label": label,
                "count": count,
                "selected_count": selected_count,
                "is_self": uin == cfg.account,
                "chat_url": format!("/chat?table={}&rowid={}#row-{}", urlencoding::encode(&latest_table), latest_rowid, latest_rowid),
            })
        })
        .collect::<Vec<_>>();
    top_senders.sort_by(|left, right| {
        right["selected_count"]
            .as_u64()
            .cmp(&left["selected_count"].as_u64())
            .then_with(|| right["count"].as_u64().cmp(&left["count"].as_u64()))
            .then_with(|| left["uin"].as_str().cmp(&right["uin"].as_str()))
    });
    top_senders.truncate(20);

    let mut conversation_counts = conversations.into_iter().collect::<Vec<_>>();
    conversation_counts
        .sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let unique_conversations = conversation_counts.len();
    let mut top_conversations = Vec::new();
    for (table, count) in &conversation_counts {
        let (conv_type, id) = crate::msg3_log_service_tables::split_table(table);
        let detail = cached_conversation_detail(cfg, con, info, detail_cache, table).ok();
        let latest_rowid = facts
            .iter()
            .find(|fact| fact.table == *table)
            .map(|fact| fact.rowid)
            .unwrap_or(1);
        top_conversations.push(json!({
            "table": table,
            "id": id,
            "type": conv_type,
            "label": detail.as_ref().map(|item| item.label.as_str()).unwrap_or(table),
            "avatar_url": detail.as_ref().map(|item| item.group_avatar_url.as_str()).unwrap_or(""),
            "count": count,
            "selected_count": conversation_selected_counts.get(table).copied().unwrap_or(0),
            "chat_url": format!("/chat?table={}&rowid={}#row-{}", urlencoding::encode(table), latest_rowid, latest_rowid),
        }));
    }
    top_conversations.sort_by(|left, right| {
        right["selected_count"]
            .as_u64()
            .cmp(&left["selected_count"].as_u64())
            .then_with(|| right["count"].as_u64().cmp(&left["count"].as_u64()))
            .then_with(|| left["table"].as_str().cmp(&right["table"].as_str()))
    });
    top_conversations.truncate(20);

    let filtered_facts = facts
        .iter()
        .filter(|fact| sender_filter.is_none_or(|sender| fact.sender_uin == sender))
        .filter(|fact| {
            sender_filters
                .as_ref()
                .is_none_or(|senders| senders.contains(fact.sender_uin.as_str()))
        })
        .filter(|fact| table_filter.is_none_or(|table| fact.table == table))
        .filter(|fact| {
            table_filters
                .as_ref()
                .is_none_or(|tables| tables.contains(fact.table.as_str()))
        })
        .filter(|fact| {
            conversation_type_filter.is_none_or(|selected| {
                crate::msg3_log_service_tables::split_table(&fact.table).0 == selected
            })
        })
        .filter(|fact| period_filter.is_none_or(|period| fact.datetime.starts_with(period)))
        .filter(|fact| {
            let date = fact.datetime.get(0..10).unwrap_or("");
            date_from_filter.is_none_or(|from| date >= from)
                && date_to_filter.is_none_or(|to| date <= to)
        })
        .collect::<Vec<_>>();
    let filtered_total = filtered_facts.len();
    let page = filtered_facts
        .into_iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let mut resolver = AssetResolver::new(&cfg.root, &cfg.account);
    let mut items = Vec::with_capacity(page.len());
    for fact in page {
        if let Ok(context) = reference_context_value(
            cfg,
            con,
            info,
            friend_social,
            detail_cache,
            &fact.table,
            fact.rowid,
            radius,
            &mut resolver,
        ) {
            items.push(json!({
                "table": fact.table,
                "rowid": fact.rowid,
                "linked_at": fact.linked_at,
                "chat_url": format!("/chat?table={}&rowid={}#row-{}", urlencoding::encode(&fact.table), fact.rowid, fact.rowid),
                "message": compact_reference_message(&context["message"]),
                "context_before": compact_reference_messages(&context["before"]),
                "context_after": compact_reference_messages(&context["after"]),
            }));
        }
    }

    let first = facts.last();
    let last = facts.first();
    Ok(json!({
        "asset_id": asset_id,
        "summary": {
            "same_image_assets": same_image_asset_ids.len(),
            "indexed_occurrences": indexed_occurrences,
            "resolved_occurrences": facts.len(),
            "missing_messages": indexed_occurrences.saturating_sub(facts.len()),
            "unique_senders": unique_senders,
            "unique_conversations": unique_conversations,
            "first_datetime": first.map(|item| item.datetime.as_str()).unwrap_or(""),
            "last_datetime": last.map(|item| item.datetime.as_str()).unwrap_or(""),
        },
        "same_image_asset_ids": same_image_asset_ids,
        "coverage": scan,
        "top_senders": top_senders,
        "top_conversations": top_conversations,
        "timeline": timeline.into_iter().map(|(period, count)| {
            let selected_count = timeline_selected_counts.get(&period).copied().unwrap_or(0);
            json!({"period": period, "count": count, "selected_count": selected_count})
        }).collect::<Vec<_>>(),
        "daily_timeline": daily_timeline.into_iter().map(|(date, count)| {
            let selected_count = daily_selected_counts.get(&date).copied().unwrap_or(0);
            json!({"date": date, "count": count, "selected_count": selected_count})
        }).collect::<Vec<_>>(),
        "filters": {"sender": sender_filter, "table": table_filter, "period": period_filter, "date_from": date_from_filter, "date_to": date_to_filter},
        "offset": offset,
        "limit": limit,
        "total": filtered_total,
        "has_more": offset.saturating_add(limit) < filtered_total,
        "items": items,
    })
    .to_string())
}

#[cfg(feature = "image-index")]
fn compact_reference_messages(messages: &Value) -> Vec<Value> {
    messages
        .as_array()
        .into_iter()
        .flatten()
        .map(compact_reference_message)
        .collect()
}

#[cfg(feature = "image-index")]
fn compact_reference_message(message: &Value) -> Value {
    let conversation = &message["conversation"];
    json!({
        "table": message["table"].clone(),
        "rowid": message["rowid"].clone(),
        "time": message["time"].clone(),
        "datetime": message["datetime"].clone(),
        "sender_uin": message["sender_uin"].clone(),
        "display_sender": message["display_sender"].clone(),
        "display_sender_line": message["display_sender_line"].clone(),
        "avatar_url": message["avatar_url"].clone(),
        "is_self": message["is_self"].clone(),
        "display_text": message["display_text"].clone(),
        "rich_nodes": message["rich_nodes"].clone(),
        "assets": message["assets"].clone(),
        "media_kind": message["media_kind"].clone(),
        "media_label": message["media_label"].clone(),
        "unmatched_reason": message["unmatched_reason"].clone(),
        "conversation": {
            "table": conversation["table"].clone(),
            "type": conversation["type"].clone(),
            "id": conversation["id"].clone(),
            "label": conversation["label"].clone(),
            "group_avatar_url": conversation["group_avatar_url"].clone(),
        }
    })
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "image-index")]
fn reference_context_value(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    friend_social: &info_storage::InfoStorage,
    detail_cache: &mut HashMap<String, Conversation>,
    table: &str,
    rowid: i64,
    radius: i64,
    asset_resolver: &mut AssetResolver<'_>,
) -> anyhow::Result<Value> {
    let meta = cached_conversation_detail(cfg, con, info, detail_cache, table)?;
    let quoted = quote_ident(table);
    let predicate = format!(
        "rowid in (select rowid from {quoted} where rowid<?1 order by rowid desc limit ?2) \
         or rowid=?1 \
         or rowid in (select rowid from {quoted} where rowid>?1 order by rowid asc limit ?2)"
    );
    let mut sender_stmt = con.prepare(&format!(
        "select distinct SenderUin from {quoted} where {predicate}"
    ))?;
    let sender_rows = sender_stmt.query_map(params![rowid, radius], |row| row.get::<_, i64>(0))?;
    let senders = sender_rows
        .filter_map(Result::ok)
        .map(|value| value.to_string())
        .collect::<HashSet<_>>();
    let members = if meta.conv_type == "group" {
        info.group_member_profiles_for(&meta.id, &senders)?
    } else {
        HashMap::new()
    };
    let contacts = merged_contact_profiles(info, friend_social, &senders)?;
    let mut stmt = con.prepare(&format!(
        "select rowid,Time,Rand,SenderUin,MsgContent,Info from {quoted} where {predicate} order by rowid"
    ))?;
    let mut rows = stmt.query(params![rowid, radius])?;
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut message = Value::Null;
    while let Some(row) = rows.next()? {
        let item = message_item(
            row,
            con,
            &quoted,
            &meta,
            &members,
            &contacts,
            &cfg.root,
            &cfg.account,
            asset_resolver,
        )?;
        let item_rowid = item.rowid;
        let value = serde_json::to_value(item)?;
        if item_rowid < rowid {
            before.push(value);
        } else if item_rowid > rowid {
            after.push(value);
        } else {
            message = value;
        }
    }
    if message.is_null() {
        anyhow::bail!("referenced message {table}:{rowid} was not found");
    }
    Ok(json!({"message": message, "before": before, "after": after}))
}

fn merged_contact_profiles(
    info: &info_storage::InfoStorage,
    friend_social: &info_storage::InfoStorage,
    senders: &HashSet<String>,
) -> anyhow::Result<HashMap<String, info_storage::ContactProfile>> {
    let mut contacts = info.contact_profiles_for(senders)?;
    let social = friend_social.friend_social_image_profiles_for(senders)?;
    for (uin, profile) in social {
        contacts
            .entry(uin)
            .and_modify(|current| {
                if current.custom_head_file.is_empty() {
                    current.custom_head_file = profile.custom_head_file.clone();
                    current.custom_head_file_field = profile.custom_head_file_field.clone();
                }
                if current.fields.is_empty() {
                    current.fields = profile.fields.clone();
                }
            })
            .or_insert(profile);
    }
    Ok(contacts)
}

fn page_senders_rowid(
    con: &Connection,
    quoted_table: &str,
    limit: i64,
    cursor: i64,
    before: bool,
) -> anyhow::Result<HashSet<String>> {
    let sql = if before {
        format!("select distinct SenderUin from (select SenderUin from {quoted_table} where rowid < ? order by rowid desc limit ?)")
    } else {
        format!("select distinct SenderUin from (select SenderUin from {quoted_table} where rowid >= ? order by rowid asc limit ?)")
    };
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(params![cursor, limit], |r| r.get::<_, i64>(0))?;
    let mut out = HashSet::new();
    for row in rows {
        out.insert(row?.to_string());
    }
    Ok(out)
}

fn row_sender(con: &Connection, quoted_table: &str, rowid: i64) -> anyhow::Result<Option<String>> {
    Ok(con
        .query_row(
            &format!("select SenderUin from {quoted_table} where rowid = ?"),
            params![rowid],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map(|v| v.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn message_item(
    row: &rusqlite::Row<'_>,
    con: &Connection,
    quoted_table: &str,
    meta: &Conversation,
    members: &HashMap<String, info_storage::GroupMemberProfile>,
    contacts: &HashMap<String, info_storage::ContactProfile>,
    root: &Path,
    account: &str,
    asset_resolver: &mut AssetResolver<'_>,
) -> anyhow::Result<MessageItem> {
    let rowid: i64 = row.get(0)?;
    let time: i64 = row.get(1)?;
    let rand: i64 = row.get(2)?;
    let sender: i64 = row.get(3)?;
    let msg: Vec<u8> = row.get(4)?;
    let info: Vec<u8> = row.get(5)?;
    let summary_map = info_summary(&info);
    let sender_uin = sender.to_string();
    let sender_name = normalize_sender_show_name(
        &summary_map
            .get("strSenderShowName")
            .cloned()
            .unwrap_or_default(),
        &sender_uin,
        account,
        &meta.id,
    );
    let receiver_name = summary_map
        .get("strRecieverShowName")
        .cloned()
        .unwrap_or_default();
    let (text, elements_json, rich_nodes_json, rich_html) = parser::parse_msgcontent_outputs(&msg);
    let mut rich_nodes: Value =
        serde_json::from_str(&rich_nodes_json).unwrap_or_else(|_| json!([]));
    let elements: Value = serde_json::from_str(&elements_json).unwrap_or_else(|_| json!([]));
    let style_meta = message_style_meta(&elements);
    let quote_refs = quote_refs_from_elements(&elements);
    annotate_quote_nodes(
        con,
        quoted_table,
        &meta.table,
        rowid,
        &quote_refs,
        &mut rich_nodes,
    );
    let mmp_items: Value = serde_json::from_str(&parser::parse_info_mmp_items_json(&info))
        .unwrap_or_else(|_| json!([]));
    attach_mmp_items_to_multi_msg(&mut rich_nodes, &mmp_items, account);
    let asset_match = match_assets_with_resolver(asset_resolver, &rich_nodes);
    let member_profile = members.get(&sender_uin).cloned().unwrap_or_default();
    let contact_profile = contacts.get(&sender_uin).cloned().unwrap_or_default();
    let (avatar_url, avatar_candidates, avatar_reason) = resolve_member_avatar(
        root,
        account,
        &sender_uin,
        &member_profile,
        &contact_profile,
    );
    let member_label = member_profile.display_name.clone();
    let merged_member = merge_member_label_with_message_info(&member_label, &sender_name);
    let system_label = if ["0", "10000", "1000000"].contains(&sender_uin.as_str()) {
        "系统消息".to_string()
    } else {
        String::new()
    };
    let is_self = sender_uin == account;
    let display_sender =
        first_nonempty(&[&merged_member, &sender_name, &system_label, &sender_uin]);
    let display_sender_line = sender_line(
        &display_sender,
        &sender_uin,
        is_self,
        !system_label.is_empty(),
    );
    let source = if is_self {
        "self"
    } else if !system_label.is_empty() {
        "system"
    } else if !merged_member.is_empty() {
        "group_member"
    } else if !sender_name.is_empty() {
        "message_info"
    } else {
        "uin"
    };
    let note = if meta.conv_type == "group" && !is_self && system_label.is_empty() {
        if !merged_member.is_empty() {
            if member_profile.title.is_empty() {
                "群成员库".to_string()
            } else {
                format!("群成员库 · 头衔: {}", member_profile.title)
            }
        } else if !sender_name.is_empty() {
            "消息Info名称".to_string()
        } else {
            "群成员昵称未命中".to_string()
        }
    } else {
        String::new()
    };
    let info_json: Value =
        serde_json::from_str(&parser::parse_info_json(&info)).unwrap_or_else(|_| json!({}));
    let info_summary_value = json!(summary_map);
    Ok(MessageItem {
        table: meta.table.clone(),
        rowid,
        time,
        datetime: iso_time(time),
        time_is_plausible: time >= 946684800,
        rand,
        sender_uin,
        sender_name,
        receiver_name,
        is_self,
        display_sender,
        display_sender_line,
        sender_role: if is_self {
            "自己"
        } else if !system_label.is_empty() {
            "系统"
        } else {
            "对方"
        }
        .to_string(),
        sender_identity_source: source.to_string(),
        sender_identity_note: note,
        member_profile: serde_json::to_value(&member_profile).unwrap_or_else(|_| json!({})),
        contact_profile: serde_json::to_value(&contact_profile).unwrap_or_else(|_| json!({})),
        avatar_url,
        avatar_candidates,
        avatar_reason,
        conversation: meta.clone_for_item(),
        indexed_text: String::new(),
        display_text: if text.is_empty() {
            "[空文本/未解析]".to_string()
        } else {
            text.clone()
        },
        text,
        rich_nodes: asset_match.rich_nodes,
        rich_html,
        style_meta,
        elements,
        elements_json,
        assets: asset_match.assets,
        media_kind: asset_match.media_kind,
        media_label: asset_match.media_label,
        candidate_paths: asset_match.candidate_paths,
        candidate_path_count: asset_match.candidate_count as i64,
        candidate_path_hit_count: asset_match.hit_count as i64,
        asset_scan_pending: false,
        unmatched_reason: asset_match.unmatched_reason,
        info_json,
        info_summary: info_summary_value,
        msgcontent_len: msg.len(),
        info_len: info.len(),
    })
}

fn sender_line(label: &str, uin: &str, is_self: bool, is_system: bool) -> String {
    if label.is_empty() || is_self || is_system {
        return label.to_string();
    }
    format!("{label}({uin})")
}

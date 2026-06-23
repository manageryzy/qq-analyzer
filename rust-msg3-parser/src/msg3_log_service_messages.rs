use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::info_storage;
use crate::msg3_log_service_assets::match_assets;
use crate::msg3_log_service_avatar::resolve_member_avatar;
use crate::msg3_log_service_config::Config;
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
        "has_prev": offset > 0,
        "has_next": offset + limit < total,
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
        )
        .map_err(|err| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                err.to_string(),
            )))
        })
    })?;
    Ok(serde_json::to_string(&item)?)
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

fn message_item(
    row: &rusqlite::Row<'_>,
    con: &Connection,
    quoted_table: &str,
    meta: &Conversation,
    members: &HashMap<String, info_storage::GroupMemberProfile>,
    contacts: &HashMap<String, info_storage::ContactProfile>,
    root: &Path,
    account: &str,
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
    let asset_match = match_assets(root, account, &rich_nodes);
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

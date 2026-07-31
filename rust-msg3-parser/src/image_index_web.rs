use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use base64::Engine;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};

use crate::image_index;
use crate::image_insights;

const DEFAULT_PAGE_SIZE: usize = 60;
const MAX_PAGE_SIZE: usize = 100;

#[derive(Clone, Debug)]
pub struct AssetOccurrenceKey {
    pub table: String,
    pub rowid: i64,
    pub linked_at: String,
}

fn manifest_path(root: &Path, account: &str) -> anyhow::Result<PathBuf> {
    let path = image_index::default_manifest_path(root, account);
    if !path.is_file() {
        anyhow::bail!("image index manifest is unavailable: {}", path.display());
    }
    Ok(path)
}

fn open_manifest(root: &Path, account: &str) -> anyhow::Result<Connection> {
    Ok(Connection::open_with_flags(
        manifest_path(root, account)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

pub fn overview(root: &Path, account: &str) -> anyhow::Result<Value> {
    let con = open_manifest(root, account)?;
    let payload_json = con
        .query_row(
            "select payload_json from image_index_overview_cache where id=1 and schema_version=1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(payload_json) = payload_json else {
        anyhow::bail!(
            "image index overview is not materialized; restart the service or rebuild the index"
        );
    };
    let mut payload = serde_json::from_str::<Value>(&payload_json)?;
    let Some(object) = payload.as_object_mut() else {
        anyhow::bail!("image index overview cache is corrupt");
    };
    object.insert("account".to_string(), json!(account));
    Ok(payload)
}

/// Cheap, persisted maintenance progress used by the monitoring page. This
/// deliberately reads the materialized overview cache instead of recounting a
/// multi-million-row manifest on every poll.
pub fn maintenance_tasks(root: &Path, account: &str) -> anyhow::Result<Value> {
    let con = open_manifest(root, account)?;
    let (active, grouped, overview_updated, payload_json) = con.query_row(
        r#"select active_asset_count,grouped_asset_count,refreshed_at,payload_json
           from image_index_overview_cache where id=1 and schema_version=1"#,
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;
    let payload: Value = serde_json::from_str(&payload_json)?;
    let clip = payload["coverage"]["clip_assets"].as_i64().unwrap_or(0);
    let sscd = payload["coverage"]["sscd_assets"].as_i64().unwrap_or(0);
    let errors = payload["health"]["errors"].as_i64().unwrap_or(0);
    let embeddable = active.saturating_sub(errors);
    let (
        scan_tables,
        incomplete_tables,
        rows_scanned,
        last_rowid,
        max_rowid,
        occurrences,
        scan_updated,
    ) = con.query_row(
        r#"select count(*),
                  coalesce(sum(case when completed=0 or linker_version<2 then 1 else 0 end),0),
                  coalesce(sum(case when linker_version>=2 then rows_scanned else 0 end),0),
                  coalesce(sum(case when linker_version>=2 then min(last_rowid,max_rowid) else 0 end),0),
                  coalesce(sum(max_rowid),0),
                  coalesce(sum(case when linker_version>=2 then occurrences_linked else 0 end),0),
                  coalesce(max(updated_at),'')
           from image_occurrence_scan_state"#,
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
            ))
        },
    )?;
    let chat_status = if scan_tables == 0 {
        "not_started"
    } else if incomplete_tables == 0 {
        "complete"
    } else {
        "running"
    };
    let popularity_task = image_insights::maintenance_progress(root, account)?;
    Ok(json!({
        "account": account,
        "tasks": [
            {"id": "image-manifest", "title": "图像清单与完全副本分组", "status": if grouped >= active && active > 0 { "complete" } else { "pending" }, "current": grouped, "total": active, "unit": "张", "updated_at": overview_updated},
            {"id": "clip-embeddings", "title": "CLIP 语义向量", "status": if clip >= embeddable && embeddable > 0 { "complete" } else { "pending" }, "current": clip, "total": embeddable, "unit": "可解码图片", "updated_at": overview_updated},
            {"id": "sscd-embeddings", "title": "SSCD 视觉副本向量", "status": if sscd >= embeddable && embeddable > 0 { "complete" } else { "pending" }, "current": sscd, "total": embeddable, "unit": "可解码图片", "updated_at": overview_updated},
            {"id": "chat-provenance", "title": "聊天图片引用链接", "status": chat_status, "current": last_rowid, "total": max_rowid, "unit": "rowid 范围（估算）", "updated_at": scan_updated,
             "details": {"tables_indexed": scan_tables, "tables_incomplete": incomplete_tables, "rows_scanned": rows_scanned, "occurrences_linked": occurrences}},
            popularity_task
        ]
    }))
}

fn page_size(params: &HashMap<String, String>) -> usize {
    params
        .get("limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE)
}

fn encode_cursor(value: &Value) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
}

fn decode_cursor(value: &str) -> anyhow::Result<Value> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn query_asset_items(
    con: &Connection,
    where_sql: &[String],
    values: &[SqlValue],
    popularity_join_sql: &str,
    reference_count_sql: &str,
    order_sql: &str,
    limit: usize,
) -> anyhow::Result<Vec<Value>> {
    let sql = format!(
        "select a.id,a.file_size,a.mtime_unix,a.width,a.height,a.sha256_hex,a.phash_hex,a.blur_score,a.quality_flags,a.source_class,a.detected_format,a.has_alpha,a.error,g.member_count,g.duplicate_bytes,(select count(*) from image_embeddings e where e.path=a.path),{reference_count_sql} as reference_count,g.member_count from image_assets a join image_exact_groups g on g.asset_id=a.id {popularity_join_sql} where {} order by {order_sql} limit ?",
        where_sql.join(" and "),
    );
    let mut query_values = values.to_vec();
    query_values.push(SqlValue::Integer(limit as i64));
    let mut stmt = con.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(query_values.iter()))?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let id = row.get::<_, i64>(0)?;
        items.push(json!({
            "id": id,
            "file_size": row.get::<_, i64>(1)?,
            "mtime_unix": row.get::<_, i64>(2)?,
            "width": row.get::<_, Option<i64>>(3)?,
            "height": row.get::<_, Option<i64>>(4)?,
            "sha256": row.get::<_, String>(5)?,
            "phash": row.get::<_, Option<String>>(6)?,
            "blur_score": row.get::<_, Option<f64>>(7)?,
            "quality_flags": row.get::<_, String>(8)?,
            "source": row.get::<_, String>(9)?,
            "format": row.get::<_, String>(10)?,
            "has_alpha": row.get::<_, i64>(11)? != 0,
            "error": row.get::<_, Option<String>>(12)?,
            "copy_count": row.get::<_, i64>(13)?,
            "duplicate_bytes": row.get::<_, i64>(14)?,
            "embedding_count": row.get::<_, i64>(15)?,
            "reference_count": row.get::<_, i64>(16)?,
            "variant_count": row.get::<_, i64>(17)?,
            "content_url": format!("/api/image-index/assets/{id}/content"),
            "thumbnail_url": format!("/api/image-index/assets/{id}/thumbnail?edge=320"),
        }));
    }
    Ok(items)
}

pub fn assets(
    root: &Path,
    account: &str,
    params: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    let con = open_manifest(root, account)?;
    let limit = page_size(params);
    let mut where_sql = vec!["a.stale=0".to_string()];
    let mut values = Vec::<SqlValue>::new();
    if let Some(value) = params.get("format").filter(|value| !value.is_empty()) {
        where_sql.push("lower(a.detected_format)=lower(?)".to_string());
        values.push(SqlValue::Text(value.clone()));
    }
    if let Some(value) = params.get("source").filter(|value| !value.is_empty()) {
        where_sql.push("a.source_class=?".to_string());
        values.push(SqlValue::Text(value.clone()));
    }
    if let Some(value) = params
        .get("quality")
        .filter(|value| !value.is_empty() && value.as_str() != "all")
    {
        for flag in value.split(',').filter(|flag| !flag.trim().is_empty()) {
            if flag == "clean" {
                where_sql.push("a.quality_flags=''".to_string());
            } else {
                where_sql.push("a.quality_flags like ?".to_string());
                values.push(SqlValue::Text(format!("%{}%", flag.trim())));
            }
        }
    }
    match params.get("embeddings").map(String::as_str).unwrap_or("") {
        "any" => where_sql
            .push("exists(select 1 from image_embeddings e where e.path=a.path)".to_string()),
        "clip" => where_sql.push(
            "exists(select 1 from image_embeddings e where e.path=a.path and e.kind='clip')"
                .to_string(),
        ),
        "sscd" => where_sql.push(
            "exists(select 1 from image_embeddings e where e.path=a.path and e.kind='sscd')"
                .to_string(),
        ),
        "both" => {
            where_sql.push(
                "exists(select 1 from image_embeddings e where e.path=a.path and e.kind='clip')"
                    .to_string(),
            );
            where_sql.push(
                "exists(select 1 from image_embeddings e where e.path=a.path and e.kind='sscd')"
                    .to_string(),
            );
        }
        "none" => where_sql
            .push("not exists(select 1 from image_embeddings e where e.path=a.path)".to_string()),
        _ => {}
    }
    let sort = match params.get("sort").map(String::as_str).unwrap_or("newest") {
        "popular" => "popular",
        "oldest" => "oldest",
        _ => "newest",
    };
    let duplicate_mode = params
        .get("duplicates")
        .or_else(|| params.get("duplicate"))
        .map(String::as_str)
        .unwrap_or("collapsed");
    match duplicate_mode {
        "all" => {}
        "variants" => where_sql.push(
            r#"a.id=g.representative_asset_id and not (
                a.quality_flags like '%thumbnail%'
                and a.phash_hex is not null and a.phash_hex<>''
                and exists (
                    select 1
                    from image_assets original indexed by image_assets_phash_idx
                    where original.stale=0
                      and original.phash_hex=a.phash_hex
                      and original.quality_flags not like '%thumbnail%'
                      and (original.error is null or original.error='')
                )
            )"#
            .to_string(),
        ),
        "duplicates" => {
            where_sql.push("a.id=g.representative_asset_id".to_string());
            where_sql.push("g.member_count>1".to_string());
        }
        "unique" => {
            where_sql.push("a.id=g.representative_asset_id".to_string());
            where_sql.push("g.member_count=1".to_string());
        }
        _ => where_sql.push("a.id=g.representative_asset_id".to_string()),
    }
    let (popularity_table, popularity_key, popularity_index) = if duplicate_mode == "all" {
        (
            "image_asset_reference_counts",
            "asset_id",
            "image_asset_reference_counts_popularity_idx",
        )
    } else {
        (
            "image_exact_reference_counts",
            "representative_asset_id",
            "image_exact_reference_counts_popularity_idx",
        )
    };
    let references_filter = params.get("references").map(String::as_str).unwrap_or("");
    match references_filter {
        "with" | "any" => where_sql.push(format!(
            "exists(select 1 from {popularity_table} filtered_popularity where filtered_popularity.{popularity_key}=a.id)"
        )),
        "without" | "none" => where_sql.push(format!(
            "not exists(select 1 from {popularity_table} filtered_popularity where filtered_popularity.{popularity_key}=a.id)"
        )),
        _ => {}
    }
    let mut cursor = None;
    if let Some(encoded_cursor) = params.get("cursor") {
        let decoded = decode_cursor(encoded_cursor)?;
        let id = decoded["id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("invalid asset cursor"))?;
        if decoded["sort"].as_str().unwrap_or("newest") != sort {
            anyhow::bail!("asset cursor does not match the selected sort order");
        }
        let references = if sort == "popular" {
            decoded["references"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("invalid popular asset cursor"))?
        } else {
            0
        };
        cursor = Some((id, references));
    }
    let mut items = if sort == "popular" {
        let requested = limit + 1;
        let mut popular_items = Vec::new();
        let cursor_references = cursor.map(|(_, references)| references);
        let allow_positive =
            !matches!(references_filter, "without" | "none") && cursor_references != Some(0);
        if allow_positive {
            let mut positive_where = where_sql.clone();
            let mut positive_values = values.clone();
            if let Some((id, references)) = cursor {
                positive_where.push(
                    "(popularity.reference_count<? or (popularity.reference_count=? and a.id<?))"
                        .to_string(),
                );
                positive_values.push(SqlValue::Integer(references));
                positive_values.push(SqlValue::Integer(references));
                positive_values.push(SqlValue::Integer(id));
            }
            let popularity_join_sql = format!(
                "join {popularity_table} popularity indexed by {popularity_index} on popularity.{popularity_key}=a.id"
            );
            popular_items = query_asset_items(
                &con,
                &positive_where,
                &positive_values,
                &popularity_join_sql,
                "popularity.reference_count",
                "popularity.reference_count desc,a.id desc",
                requested,
            )?;
        }
        let allow_zero = !matches!(references_filter, "with" | "any");
        if allow_zero && popular_items.len() < requested {
            let mut zero_where = where_sql.clone();
            let mut zero_values = values.clone();
            zero_where.push(format!(
                "not exists(select 1 from {popularity_table} zero_popularity where zero_popularity.{popularity_key}=a.id)"
            ));
            if let Some((id, references)) = cursor {
                if references == 0 {
                    zero_where.push("a.id<?".to_string());
                    zero_values.push(SqlValue::Integer(id));
                }
            }
            let remaining = requested - popular_items.len();
            popular_items.extend(query_asset_items(
                &con,
                &zero_where,
                &zero_values,
                "",
                "0",
                "a.id desc",
                remaining,
            )?);
        }
        popular_items
    } else {
        if let Some((id, _)) = cursor {
            where_sql.push(if sort == "oldest" {
                "a.id>?".to_string()
            } else {
                "a.id<?".to_string()
            });
            values.push(SqlValue::Integer(id));
        }
        let reference_count_sql = format!(
            "coalesce((select cached_popularity.reference_count from {popularity_table} cached_popularity where cached_popularity.{popularity_key}=a.id),0)"
        );
        let order_sql = if sort == "oldest" {
            "a.id asc"
        } else {
            "a.id desc"
        };
        query_asset_items(
            &con,
            &where_sql,
            &values,
            "",
            &reference_count_sql,
            order_sql,
            limit + 1,
        )?
    };
    let has_more = items.len() > limit;
    if has_more {
        items.pop();
    }
    let next_cursor = if has_more {
        items.last().and_then(|item| {
            let id = item["id"].as_i64()?;
            Some(encode_cursor(&json!({
                "v": 2,
                "id": id,
                "sort": sort,
                "references": item["reference_count"],
            })))
        })
    } else {
        None
    };
    Ok(json!({
        "items": items,
        "next_cursor": next_cursor,
        "limit": limit,
        "duplicates": duplicate_mode,
        "sort": sort,
    }))
}

pub fn asset_detail(root: &Path, account: &str, id: i64) -> anyhow::Result<Option<Value>> {
    let con = open_manifest(root, account)?;
    let mut detail = con
        .query_row(
            "select a.id,a.path,a.source_root,a.file_size,a.mtime_unix,a.sha256_hex,a.phash_hex,a.phash_algo,a.width,a.height,a.blur_score,a.blur_algo,a.quality_flags,a.source_class,a.detected_format,a.has_alpha,a.orientation_applied,a.fingerprint_version,a.indexed_at,a.stale,a.error,g.group_key,g.representative_asset_id,g.member_count,g.total_bytes,g.duplicate_bytes from image_assets a left join image_exact_groups g on g.asset_id=a.id where a.id=?1",
            [id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "diagnostics": {"path": row.get::<_, String>(1)?, "source_root": row.get::<_, String>(2)?},
                    "file_size": row.get::<_, i64>(3)?, "mtime_unix": row.get::<_, i64>(4)?,
                    "sha256": row.get::<_, String>(5)?, "phash": row.get::<_, Option<String>>(6)?, "phash_algo": row.get::<_, String>(7)?,
                    "width": row.get::<_, Option<i64>>(8)?, "height": row.get::<_, Option<i64>>(9)?,
                    "blur_score": row.get::<_, Option<f64>>(10)?, "blur_algo": row.get::<_, String>(11)?,
                    "quality_flags": row.get::<_, String>(12)?, "source": row.get::<_, String>(13)?, "format": row.get::<_, String>(14)?,
                    "has_alpha": row.get::<_, i64>(15)? != 0, "orientation_applied": row.get::<_, i64>(16)? != 0,
                    "fingerprint_version": row.get::<_, String>(17)?, "indexed_at": row.get::<_, String>(18)?,
                    "stale": row.get::<_, i64>(19)? != 0, "error": row.get::<_, Option<String>>(20)?,
                    "exact_copy": {"group": row.get::<_, Option<String>>(21)?, "representative_id": row.get::<_, Option<i64>>(22)?, "count": row.get::<_, Option<i64>>(23)?.unwrap_or(1), "total_bytes": row.get::<_, Option<i64>>(24)?.unwrap_or(0), "duplicate_bytes": row.get::<_, Option<i64>>(25)?.unwrap_or(0)},
                    "content_url": format!("/api/image-index/assets/{id}/content"),
                    "thumbnail_url": format!("/api/image-index/assets/{id}/thumbnail?edge=640"),
                }))
            },
        )
        .optional()?;
    let Some(detail) = detail.as_mut() else {
        return Ok(None);
    };
    let mut embeddings = Vec::new();
    let path = detail["diagnostics"]["path"].as_str().unwrap_or("");
    let mut stmt = con.prepare("select kind,model,dim,sketch64_hex,bucket12,updated_at from image_embeddings where path=?1 order by kind,model")?;
    let rows = stmt.query_map([path], |row| {
        Ok(json!({"kind": row.get::<_, String>(0)?, "model": row.get::<_, String>(1)?, "dimensions": row.get::<_, i64>(2)?, "sketch64": row.get::<_, String>(3)?, "bucket12": row.get::<_, Option<i64>>(4)?, "updated_at": row.get::<_, String>(5)?}))
    })?;
    embeddings.extend(rows.collect::<Result<Vec<_>, _>>()?);
    let feature_counts = json!({
        "tile_hashes": con.query_row("select count(*) from image_asset_tile_hashes where asset_id=?1", [id], |row| row.get::<_, i64>(0))?,
        "local_features": con.query_row("select coalesce(sum(descriptor_count),0) from image_asset_local_features where asset_id=?1", [id], |row| row.get::<_, i64>(0))?,
    });
    let mut copies = Vec::new();
    let group = detail["exact_copy"]["group"].as_str().unwrap_or("");
    let mut stmt = con.prepare("select a.id,a.path,a.file_size,a.width,a.height,g.representative_asset_id=a.id from image_exact_groups g join image_assets a on a.id=g.asset_id where g.group_key=?1 order by a.id")?;
    let rows = stmt.query_map([group], |row| {
        let copy_id = row.get::<_, i64>(0)?;
        Ok(json!({"id": copy_id, "diagnostics": {"path": row.get::<_, String>(1)?}, "file_size": row.get::<_, i64>(2)?, "width": row.get::<_, Option<i64>>(3)?, "height": row.get::<_, Option<i64>>(4)?, "representative": row.get::<_, bool>(5)?, "thumbnail_url": format!("/api/image-index/assets/{copy_id}/thumbnail?edge=160")}))
    })?;
    copies.extend(rows.collect::<Result<Vec<_>, _>>()?);
    detail["embeddings"] = Value::Array(embeddings);
    detail["features"] = feature_counts;
    detail["copies"] = Value::Array(copies);
    Ok(Some(detail.clone()))
}

pub fn occurrences(
    root: &Path,
    account: &str,
    id: i64,
    params: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    let con = open_manifest(root, account)?;
    let limit = page_size(params);
    let mut values = vec![SqlValue::Integer(id)];
    let mut cursor_sql = String::new();
    if let Some(cursor) = params.get("cursor") {
        let value = decode_cursor(cursor)?;
        let table = value["table"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invalid occurrence cursor"))?;
        let rowid = value["rowid"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("invalid occurrence cursor"))?;
        cursor_sql = " and (occurrence.conversation_table>? or (occurrence.conversation_table=? and occurrence.message_rowid>?))".to_string();
        values.push(SqlValue::Text(table.to_string()));
        values.push(SqlValue::Text(table.to_string()));
        values.push(SqlValue::Integer(rowid));
    }
    values.push(SqlValue::Integer((limit + 1) as i64));
    // An asset detail page represents an exact-copy group. References attached
    // to a non-representative file must therefore remain visible when the UI
    // opens the representative (and vice versa).
    let sql = format!(
        r#"with selected_group as materialized (
               select a.id selected_id,
                      coalesce(g.representative_asset_id, a.id) representative_id
               from image_assets a
               left join image_exact_groups g on g.asset_id=a.id
               where a.id=?1 and a.stale=0
           ), group_assets as (
               select selected_id id from selected_group
               union
               select grouped.asset_id
               from selected_group selected
               cross join image_exact_groups grouped
               where grouped.representative_asset_id=selected.representative_id
           )
           select occurrence.conversation_table,
                  occurrence.message_rowid,
                  max(occurrence.linked_at)
           from group_assets grouped
           cross join image_asset_occurrences occurrence indexed by sqlite_autoindex_image_asset_occurrences_1
           where occurrence.asset_id=grouped.id{cursor_sql}
           group by occurrence.conversation_table,occurrence.message_rowid
           order by occurrence.conversation_table,occurrence.message_rowid
           limit ?"#
    );
    let mut stmt = con.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
        let table = row.get::<_, String>(0)?;
        let rowid = row.get::<_, i64>(1)?;
        Ok(json!({"table": table, "rowid": rowid, "linked_at": row.get::<_, String>(2)?, "chat_url": format!("/chat?table={}&rowid={}#row-{}", urlencoding::encode(&table), rowid, rowid)}))
    })?;
    let mut items = rows.collect::<Result<Vec<_>, _>>()?;
    let has_more = items.len() > limit;
    if has_more {
        items.pop();
    }
    let next_cursor = if has_more {
        items.last().map(|item| {
            encode_cursor(&json!({"v": 1, "table": item["table"], "rowid": item["rowid"]}))
        })
    } else {
        None
    };
    let scan = occurrence_scan_value(&con)?;
    Ok(json!({"items": items, "next_cursor": next_cursor, "limit": limit, "scan": scan}))
}

/// Returns every materialized occurrence for the selected exact-copy group.
/// Message metadata and context live in the prepared chat DB and are enriched
/// by the service layer; the image manifest intentionally remains provenance-
/// only so normal browsing never writes a second copy of chat content.
pub fn occurrence_keys(
    root: &Path,
    account: &str,
    id: i64,
) -> anyhow::Result<(Vec<AssetOccurrenceKey>, Value)> {
    occurrence_keys_for_assets(root, account, &[id])
}

pub fn occurrence_keys_for_assets(
    root: &Path,
    account: &str,
    asset_ids: &[i64],
) -> anyhow::Result<(Vec<AssetOccurrenceKey>, Value)> {
    if asset_ids.is_empty() {
        anyhow::bail!("at least one image asset id is required");
    }
    let con = open_manifest(root, account)?;
    let selected_values = (1..=asset_ids.len())
        .map(|index| format!("(?{index})"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        r#"with selected_assets(id) as (values {selected_values}),
           selected_group as materialized (
               select a.id selected_id,
                      coalesce(g.representative_asset_id, a.id) representative_id
               from image_assets a
               join selected_assets selected on selected.id=a.id
               left join image_exact_groups g on g.asset_id=a.id
               where a.stale=0
           ), group_assets as (
               select selected_id id from selected_group
               union
               select grouped.asset_id
               from selected_group selected
               cross join image_exact_groups grouped
               where grouped.representative_asset_id=selected.representative_id
           )
           select occurrence.conversation_table,
                  occurrence.message_rowid,
                  max(occurrence.linked_at)
           from group_assets grouped
           cross join image_asset_occurrences occurrence indexed by sqlite_autoindex_image_asset_occurrences_1
           where occurrence.asset_id=grouped.id
           group by occurrence.conversation_table,occurrence.message_rowid
           order by occurrence.conversation_table,occurrence.message_rowid"#
    );
    let mut stmt = con.prepare(&sql)?;
    let values = asset_ids
        .iter()
        .copied()
        .map(SqlValue::Integer)
        .collect::<Vec<_>>();
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
        Ok(AssetOccurrenceKey {
            table: row.get(0)?,
            rowid: row.get(1)?,
            linked_at: row.get(2)?,
        })
    })?;
    let keys = rows.collect::<Result<Vec<_>, _>>()?;
    Ok((keys, occurrence_scan_value(&con)?))
}

fn occurrence_scan_value(con: &Connection) -> anyhow::Result<Value> {
    Ok(con.query_row(
        r#"select count(*),
                  coalesce(sum(case when completed=0 then 1 else 0 end),0),
                  coalesce(sum(case when max_rowid>last_rowid then max_rowid-last_rowid else 0 end),0),
                  coalesce(max(updated_at),''),
                  coalesce(sum(rows_scanned),0),
                  coalesce(sum(max_rowid),0),
                  coalesce(sum(occurrences_linked),0)
           from image_occurrence_scan_state"#,
        [],
        |row| {
            let tables_indexed = row.get::<_, i64>(0)?;
            let tables_incomplete = row.get::<_, i64>(1)?;
            Ok(json!({
                "complete": tables_indexed > 0 && tables_incomplete == 0,
                "tables_indexed": tables_indexed,
                "tables_incomplete": tables_incomplete,
                "pending_rowid_span": row.get::<_, i64>(2)?,
                "updated_at": row.get::<_, String>(3)?,
                "rows_scanned": row.get::<_, i64>(4)?,
                "estimated_rowid_span": row.get::<_, i64>(5)?,
                "occurrences_linked": row.get::<_, i64>(6)?,
            }))
        },
    )?)
}

pub fn asset_path(root: &Path, account: &str, id: i64) -> anyhow::Result<Option<PathBuf>> {
    let con = open_manifest(root, account)?;
    Ok(con
        .query_row(
            "select path from image_assets where id=?1 and stale=0",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(PathBuf::from)
        .map(|path| image_index::manifest_path_for_host(&path)))
}

/// Avoids sending an asset with no materialized fingerprint into the bounded
/// inference actor. On a manifest-stage-only index, every downstream signal
/// would be unavailable and pHash fallback work can otherwise scan millions of
/// pending rows before discovering that fact.
pub fn pending_similarity_report(
    root: &Path,
    account: &str,
    id: i64,
) -> anyhow::Result<Option<Value>> {
    let con = open_manifest(root, account)?;
    let quality_flags = con
        .query_row(
            "select quality_flags from image_assets where id=?1 and stale=0",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(quality_flags) = quality_flags else {
        return Ok(None);
    };
    if !quality_flags
        .split(',')
        .map(str::trim)
        .any(|flag| flag == "manifest_pending")
    {
        return Ok(None);
    }
    Ok(Some(json!({
        "account": account,
        "results": [],
        "unavailable": [
            {
                "signal": "pixel_fingerprint",
                "reason": "该图片仍在等待 pHash 与局部结构指纹索引"
            },
            {
                "signal": "copy_descriptor:sscd_vec",
                "reason": "该图片仍在等待 SSCD 视觉副本向量索引"
            },
            {
                "signal": "semantic_descriptor:clip_vec",
                "reason": "该图片仍在等待 CLIP 语义向量索引"
            }
        ]
    })))
}

pub fn ids_for_paths(
    root: &Path,
    account: &str,
    paths: &HashSet<String>,
) -> anyhow::Result<HashMap<String, i64>> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    let con = open_manifest(root, account)?;
    let mut found = HashMap::new();
    // With several IN values SQLite preferred the low-cardinality `stale`
    // index on multi-million-row manifests, turning chat-page enrichment into
    // a full active-index scan. A reused point-lookup statement reliably uses
    // the unique path index and keeps the work proportional to the page.
    let mut stmt = con.prepare("select id from image_assets where path=?1 and stale=0")?;
    let lookup_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for path in paths {
        for candidate in manifest_path_candidates(&lookup_root, path) {
            if let Some(id) = stmt
                .query_row([candidate], |row| row.get::<_, i64>(0))
                .optional()?
            {
                found.insert(path.clone(), id);
                break;
            }
        }
    }
    Ok(found)
}

/// Returns the small set of filesystem spellings that may identify one asset.
///
/// Chat parsing commonly yields a process-relative WSL path such as
/// `../123/Image/a.jpg`, while manifests built on Windows retain the extended
/// `\\?\D:\...` spelling. Keep this as point lookups instead of normalizing in
/// SQL: the manifest may contain millions of rows and its unique path index is
/// deliberately the hot path for chat-page enrichment.
pub(crate) fn manifest_path_candidates(root: &Path, path: &str) -> Vec<String> {
    fn add_spellings(candidates: &mut HashSet<String>, path: &str) {
        candidates.insert(path.to_string());
        candidates.insert(path.replace('/', "\\"));
        candidates.insert(path.replace('\\', "/"));

        let slash_path = path.replace('\\', "/");
        let mut parts = slash_path.trim_start_matches('/').splitn(3, '/');
        if parts.next() == Some("mnt") {
            if let (Some(drive), Some(suffix)) = (parts.next(), parts.next()) {
                if drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic() {
                    let windows = format!(
                        "{}:\\{}",
                        drive.to_ascii_uppercase(),
                        suffix.replace('/', "\\")
                    );
                    candidates.insert(windows.clone());
                    candidates.insert(format!(r"\\?\{windows}"));
                }
            }
        }

        let backslash_path = path.replace('/', "\\");
        if let Some(stripped) = backslash_path
            .strip_prefix(r"\\?\")
            .or_else(|| backslash_path.strip_prefix(r"\??\"))
        {
            candidates.insert(stripped.to_string());
        } else if backslash_path.as_bytes().get(1) == Some(&b':') {
            candidates.insert(format!(r"\\?\{backslash_path}"));
        }
    }

    let mut candidates = HashSet::new();
    add_spellings(&mut candidates, path);
    if !path.contains(['/', '\\']) {
        return candidates.into_iter().collect();
    }

    let source = Path::new(path);
    if source.is_absolute() {
        add_spellings(&mut candidates, &source.to_string_lossy());
    } else if let Ok(without_parent) = source.strip_prefix("..") {
        // AssetResolver emits paths relative to the analyzer directory while
        // `root` names its parent workspace. Stripping that one leading parent
        // gives a lexical absolute path without stat-ing every chat image.
        add_spellings(
            &mut candidates,
            &root.join(without_parent).to_string_lossy(),
        );
    } else {
        add_spellings(&mut candidates, &root.join(source).to_string_lossy());
    }
    candidates.into_iter().collect()
}

pub fn sanitize_search_report(
    root: &Path,
    account: &str,
    report: &mut Value,
) -> anyhow::Result<()> {
    let mut paths = HashSet::new();
    for result in report["results"].as_array().into_iter().flatten() {
        if let Some(path) = result["path"].as_str() {
            paths.insert(path.to_string());
        }
    }
    let ids = ids_for_paths(root, account, &paths)?;
    let con = open_manifest(root, account)?;
    let mut group_stmt = con.prepare(
        "select representative_asset_id,member_count from image_exact_groups where asset_id=?1",
    )?;
    let mut groups = HashMap::<i64, (i64, i64)>::new();
    for id in ids.values().copied() {
        if groups.contains_key(&id) {
            continue;
        }
        let group = group_stmt
            .query_row([id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .optional()?
            .unwrap_or((id, 1));
        groups.insert(id, group);
    }
    if let Some(results) = report["results"].as_array_mut() {
        results.retain_mut(|result| {
            let path = result["path"].as_str().unwrap_or("").to_string();
            let Some(id) = ids.get(&path).copied() else {
                return false;
            };
            let (representative_id, copy_count) = groups.get(&id).copied().unwrap_or((id, 1));
            if let Some(object) = result.as_object_mut() {
                object.remove("path");
                object.remove("href");
                object.insert("id".to_string(), json!(id));
                object.insert(
                    "exact_representative_id".to_string(),
                    json!(representative_id),
                );
                object.insert("copy_count".to_string(), json!(copy_count));
                object.insert(
                    "content_url".to_string(),
                    json!(format!("/api/image-index/assets/{id}/content")),
                );
                object.insert(
                    "thumbnail_url".to_string(),
                    json!(format!("/api/image-index/assets/{id}/thumbnail?edge=320")),
                );
            }
            true
        });
    }
    if let Some(query) = report.get_mut("query").and_then(Value::as_object_mut) {
        query.remove("path");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_are_opaque_and_round_trip() {
        let cursor = encode_cursor(&json!({"v": 1, "id": 42}));
        assert!(!cursor.contains("42"));
        assert_eq!(decode_cursor(&cursor).unwrap()["id"], 42);
    }

    #[test]
    fn page_size_is_bounded() {
        let mut params = HashMap::new();
        params.insert("limit".to_string(), "9999".to_string());
        assert_eq!(page_size(&params), MAX_PAGE_SIZE);
    }

    #[test]
    fn relative_chat_paths_bridge_to_windows_manifest_paths_without_io() {
        let candidates = manifest_path_candidates(
            Path::new("/mnt/d/QQData"),
            "../1234567890/Image/Group2/a.jpg",
        );
        assert!(candidates
            .iter()
            .any(|candidate| candidate == r"\\?\D:\QQData\1234567890\Image\Group2\a.jpg"));
    }

    #[test]
    fn gallery_filters_persisted_embedding_kinds_and_keeps_zero_reference_assets() {
        let root = std::env::temp_dir().join(format!(
            "qq-gallery-filters-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let account = "1";
        let manifest = image_index::default_manifest_path(&root, account);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        drop(Connection::open(&manifest).unwrap());
        image_index::prepare_web_manifest_path(&manifest).unwrap();
        let mut con = Connection::open(&manifest).unwrap();
        con.execute(
            r#"insert into image_assets(
                   id,path,sha256_hex,phash_hex,file_size,quality_flags,stale
               ) values(1,'original.png','one','same',100,'',0)"#,
            [],
        )
        .unwrap();
        con.execute(
            r#"insert into image_assets(
                   id,path,sha256_hex,phash_hex,file_size,quality_flags,stale
               ) values(2,'thumb.png','two','same',10,'thumbnail',0)"#,
            [],
        )
        .unwrap();
        for kind in ["clip", "sscd"] {
            con.execute(
                "insert into image_embeddings(path,kind,model,dim,vec) values('original.png',?1,'test',1,x'00000000')",
                [kind],
            )
            .unwrap();
        }
        con.execute(
            "insert into image_asset_occurrences(asset_id,conversation_table,message_rowid) values(1,'group_1',1)",
            [],
        )
        .unwrap();
        image_index::refresh_exact_groups(&mut con).unwrap();
        image_index::refresh_overview_cache(&mut con).unwrap();
        image_index::refresh_overview_reference_cache(&mut con).unwrap();
        drop(con);

        let both = assets(
            &root,
            account,
            &HashMap::from([
                ("embeddings".to_string(), "both".to_string()),
                ("duplicates".to_string(), "all".to_string()),
            ]),
        )
        .unwrap();
        assert_eq!(both["items"].as_array().unwrap().len(), 1);
        assert_eq!(both["items"][0]["id"], 1);

        let without = assets(
            &root,
            account,
            &HashMap::from([
                ("sort".to_string(), "popular".to_string()),
                ("references".to_string(), "without".to_string()),
                ("duplicates".to_string(), "all".to_string()),
            ]),
        )
        .unwrap();
        assert_eq!(without["items"].as_array().unwrap().len(), 1);
        assert_eq!(without["items"][0]["id"], 2);
        assert_eq!(without["items"][0]["reference_count"], 0);

        let first_popular_page = assets(
            &root,
            account,
            &HashMap::from([
                ("sort".to_string(), "popular".to_string()),
                ("duplicates".to_string(), "all".to_string()),
                ("limit".to_string(), "1".to_string()),
            ]),
        )
        .unwrap();
        assert_eq!(first_popular_page["items"][0]["id"], 1);
        assert_eq!(first_popular_page["items"][0]["reference_count"], 1);
        let second_popular_page = assets(
            &root,
            account,
            &HashMap::from([
                ("sort".to_string(), "popular".to_string()),
                ("duplicates".to_string(), "all".to_string()),
                ("limit".to_string(), "1".to_string()),
                (
                    "cursor".to_string(),
                    first_popular_page["next_cursor"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(second_popular_page["items"][0]["id"], 2);
        assert_eq!(second_popular_page["items"][0]["reference_count"], 0);

        for sort in ["newest", "popular"] {
            let variants = assets(
                &root,
                account,
                &HashMap::from([
                    ("sort".to_string(), sort.to_string()),
                    ("duplicates".to_string(), "variants".to_string()),
                ]),
            )
            .unwrap();
            assert_eq!(variants["items"].as_array().unwrap().len(), 1);
            assert_eq!(variants["items"][0]["id"], 1);
        }
        let _ = std::fs::remove_dir_all(root);
    }
}

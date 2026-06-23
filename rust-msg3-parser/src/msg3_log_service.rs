use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::json;
use tiny_http::{Header, Response, Server, StatusCode};

use msg3_richtext_parser_rs::catalog;
use msg3_richtext_parser_rs::info_storage;
use msg3_richtext_parser_rs::msg3_log_service_asset_http::serve_asset_response;
use msg3_richtext_parser_rs::msg3_log_service_config::{parse_args, Config};
use msg3_richtext_parser_rs::msg3_log_service_conversations::{
    conversation_detail_json, conversation_details_json, conversations_json,
};
use msg3_richtext_parser_rs::msg3_log_service_frontend::app_html;
use msg3_richtext_parser_rs::msg3_log_service_http::{parse_query, split_url};
use msg3_richtext_parser_rs::msg3_log_service_messages::{message_detail_json, messages_json};
use msg3_richtext_parser_rs::msg3_log_service_models::{Conversation, ConversationLight};

pub fn run() -> anyhow::Result<()> {
    let cfg = parse_args()?;
    let server =
        Server::http((cfg.host.as_str(), cfg.port)).map_err(|err| anyhow::anyhow!("{err}"))?;
    println!("Rust Msg3 log service http://{}:{}/", cfg.host, cfg.port);
    println!("account={}", cfg.account);
    println!("db={}", cfg.db.display());
    let con = Connection::open_with_flags(&cfg.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let info = info_storage::InfoStorage::new(cfg.info_root.clone(), cfg.info_key_log.clone());
    let friend_social =
        info_storage::InfoStorage::new(cfg.friend_social_root.clone(), cfg.info_key_log.clone());
    let mut light_cache: HashMap<String, ConversationLight> = HashMap::new();
    let mut detail_cache: HashMap<String, Conversation> = HashMap::new();
    for request in server.incoming_requests() {
        let url = request.url().to_string();
        let (path, _) = split_url(&url);
        let response = if path.starts_with("/asset/") {
            match serve_asset_response(&cfg.root, path) {
                Ok(response) => response,
                Err(err) => Response::from_string(json!({"error": err.to_string()}).to_string())
                    .with_status_code(StatusCode(404))
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                            .unwrap(),
                    ),
            }
        } else {
            match handle(
                &cfg,
                &con,
                &info,
                &friend_social,
                &mut light_cache,
                &mut detail_cache,
                &url,
            ) {
                Ok((content_type, body)) => Response::from_string(body)
                    .with_header(Header::from_bytes("Content-Type", content_type).unwrap()),
                Err(err) => Response::from_string(json!({"error": err.to_string()}).to_string())
                    .with_status_code(StatusCode(500))
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                            .unwrap(),
                    ),
            }
        };
        let _ = request.respond(response);
    }
    Ok(())
}

fn handle(
    cfg: &Config,
    con: &Connection,
    info: &info_storage::InfoStorage,
    friend_social: &info_storage::InfoStorage,
    light_cache: &mut HashMap<String, ConversationLight>,
    detail_cache: &mut HashMap<String, Conversation>,
    url: &str,
) -> anyhow::Result<(&'static str, String)> {
    let (path, query) = split_url(url);
    let q = parse_query(query);
    match path {
        "/" => Ok(("text/html; charset=utf-8", app_html())),
        "/api/status" => Ok((
            "application/json; charset=utf-8",
            status_json(cfg, con, info)?,
        )),
        "/api/conversations" => Ok((
            "application/json; charset=utf-8",
            conversations_json(&cfg.root, &cfg.account, con, info, light_cache, &q)?,
        )),
        "/api/conversation_detail" => Ok((
            "application/json; charset=utf-8",
            conversation_detail_json(cfg, con, info, detail_cache, &q)?,
        )),
        "/api/conversation_details" => Ok((
            "application/json; charset=utf-8",
            conversation_details_json(cfg, con, info, detail_cache, &q)?,
        )),
        "/api/messages" => Ok((
            "application/json; charset=utf-8",
            messages_json(cfg, con, info, friend_social, &q)?,
        )),
        "/api/message_detail" => Ok((
            "application/json; charset=utf-8",
            message_detail_json(cfg, con, info, friend_social, &q)?,
        )),
        p if p.starts_with("/asset/") => Ok((
            "text/plain; charset=utf-8",
            "asset route handled before API dispatch".to_string(),
        )),
        _ => Ok(("text/plain; charset=utf-8", "not found".to_string())),
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
        |r| r.get(0),
    )?;
    Ok(json!({
        "server": "rust",
        "tables": tables,
        "catalog": {
            "path": cfg.catalog,
            "available": cfg.catalog.is_file(),
            "summary": if cfg.catalog.is_file() { catalog::summarize(&cfg.catalog).ok() } else { None },
        },
        "data_sources": {
            "msg3_db": {
                "path": cfg.db,
                "source": cfg.db_source,
                "exists": cfg.db.is_file()
            },
            "infostorage": {
                "path": cfg.info_root,
                "source": cfg.info_root_source,
                "exists": cfg.info_root.is_dir()
            }
        },
        "infostorage": {
            "available": info.available(),
            "mode": "on_demand_no_disk_cache"
        },
        "asset_scan": {
            "running": false,
            "complete": true,
            "mode": "on_demand",
            "indexed_assets": 0,
            "active_assets": 0,
            "error": null
        }
    })
    .to_string())
}

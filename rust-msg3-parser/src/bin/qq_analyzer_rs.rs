use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use rusqlite::Connection;
use serde_json::json;

use msg3_richtext_parser_rs::{
    asset_audit, capture, catalog, config, credentials, db_analysis, html_check, info_storage,
    inventory, migration_audit, msg3_index, msg3_log_service, msg3_parser as parser, msg3_samples,
    preprocess, snapshot, sqlite_tools,
};

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_usage();
        return Ok(());
    };
    match command.as_str() {
        "assets" => cmd_assets(args.collect()),
        "inventory" => cmd_inventory(args.collect()),
        "capture" => cmd_capture(args.collect()),
        "catalog" => cmd_catalog(args.collect()),
        "credentials" => cmd_credentials(args.collect()),
        "db" => cmd_db(args.collect()),
        "html" => cmd_html(args.collect()),
        "info" => cmd_info(args.collect()),
        "migration" => cmd_migration(args.collect()),
        "msg3" => cmd_msg3(args.collect()),
        "preprocess" => cmd_preprocess(args.collect()),
        "snapshot" => cmd_snapshot(args.collect()),
        "serve" => msg3_log_service::run(),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => anyhow::bail!("unknown command: {command}"),
    }
}

fn cmd_snapshot(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!(
            "usage: qq_analyzer_rs snapshot <test-clone> --input <src> --out <dst> [--force]"
        );
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "test-clone" => {
            let opts = CommonOpts::parse(rest)?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <src> is required"))?;
            let output = opts
                .output
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--out <dst> is required"))?;
            let result = snapshot::create_working_copy(&input, &output, opts.force)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "input": input,
                    "output": output,
                    "method": result.method,
                    "bytes": result.bytes,
                    "fallback_reason": result.fallback_reason,
                }))?
            );
            Ok(())
        }
        _ => anyhow::bail!("unknown snapshot subcommand: {subcommand}"),
    }
}

fn cmd_migration(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!("usage: qq_analyzer_rs migration <audit-python> [options]");
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "audit-python" => {
            let opts = CommonOpts::parse(rest)?;
            let report = migration_audit::audit_python(&opts.root)?;
            write_or_print_json(opts.output.as_ref(), &report)?;
            if opts.strict && report.totals.unknown_python > 0 {
                anyhow::bail!(
                    "python audit found {} unclassified Python files",
                    report.totals.unknown_python
                );
            }
            Ok(())
        }
        _ => anyhow::bail!("unknown migration subcommand: {subcommand}"),
    }
}

fn cmd_msg3(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!(
            "usage: qq_analyzer_rs msg3 <row-parse|row-probe|info-parse|index-query|export-samples> [options]"
        );
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "row-parse" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let table = opts
                .tables
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("--table <msg3-table> is required"))?;
            let rowid = opts
                .rowid
                .ok_or_else(|| anyhow::anyhow!("--rowid <rowid> is required"))?;
            let db = opts
                .input
                .clone()
                .unwrap_or_else(|| config::msg3_db(&opts.root, &account));
            let data = msgcontent_by_rowid(&db, &table, rowid)?;
            let (text, elements_json, rich_nodes_json, rich_html) =
                parser::parse_msgcontent_outputs(&data);
            let out = json!({
                "account": account,
                "db": db,
                "table": table,
                "rowid": rowid,
                "msgcontent_len": data.len(),
                "text": text,
                "rich_nodes": serde_json::from_str::<serde_json::Value>(&rich_nodes_json)?,
                "elements": serde_json::from_str::<serde_json::Value>(&elements_json)?,
                "rich_html": rich_html,
            });
            write_or_print_json(opts.output.as_ref(), &out)
        }
        "row-probe" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let table = opts
                .tables
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("--table <msg3-table> is required"))?;
            let rowid = opts
                .rowid
                .ok_or_else(|| anyhow::anyhow!("--rowid <rowid> is required"))?;
            let db = opts
                .input
                .clone()
                .unwrap_or_else(|| config::msg3_db(&opts.root, &account));
            let data = msgcontent_by_rowid(&db, &table, rowid)?;
            let start = opts.start.unwrap_or(0);
            let len = opts.byte_len.unwrap_or(512);
            let out = json!({
                "account": account,
                "db": db,
                "table": table,
                "rowid": rowid,
                "msgcontent_len": data.len(),
                "start": start,
                "len": len,
                "hex_window": hex_window(&data, start, len),
            });
            write_or_print_json(opts.output.as_ref(), &out)
        }
        "info-parse" => {
            let opts = CommonOpts::parse(rest)?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <info.bin> is required"))?;
            let data = std::fs::read(&input).with_context(|| input.display().to_string())?;
            let info_json = parser::parse_info_json(&data);
            let mmp_items_json = parser::parse_info_mmp_items_json(&data);
            let out = json!({
                "input": input,
                "info_len": data.len(),
                "info": serde_json::from_str::<serde_json::Value>(&info_json)?,
                "mmp_items": serde_json::from_str::<serde_json::Value>(&mmp_items_json)?,
            });
            write_or_print_json(opts.output.as_ref(), &out)
        }
        "index-query" => {
            let opts = CommonOpts::parse(rest)?;
            let db = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <Msg3.0index.db> is required"))?;
            let report = msg3_index::query_index(&msg3_index::IndexQueryOptions {
                db,
                conversation_account: opts.conversation_account,
                like: opts.like,
                fts_match: opts.fts_match,
                limit: opts.limit.unwrap_or(30),
            })?;
            write_or_print_json(opts.output.as_ref(), &report)
        }
        "export-samples" => {
            let opts = CommonOpts::parse(rest)?;
            let db = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <Msg3.0.db> is required"))?;
            let out = opts
                .output
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--out <samples.tsv> is required"))?;
            let known_rows = opts
                .known_rows
                .iter()
                .map(|spec| msg3_samples::parse_known_row(spec))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let summary = msg3_samples::export_samples(&msg3_samples::ExportSamplesOptions {
                db,
                out,
                rows_per_table: opts.rows_per_table.unwrap_or(50),
                tables: opts.tables,
                all_message_tables: opts.all_message_tables,
                max_tables: opts.max_tables,
                known_rows,
            })?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        _ => anyhow::bail!("unknown msg3 subcommand: {subcommand}"),
    }
}

fn msgcontent_by_rowid(db: &PathBuf, table: &str, rowid: i64) -> anyhow::Result<Vec<u8>> {
    let con = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| db.display().to_string())?;
    let sql = format!(
        "SELECT MsgContent FROM {} WHERE rowid=?",
        quote_ident(table)
    );
    con.query_row(&sql, [rowid], |row| row.get(0))
        .with_context(|| format!("{} rowid={rowid}", table))
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn hex_window(data: &[u8], start: usize, len: usize) -> String {
    let end = data.len().min(start.saturating_add(len));
    let mut out = String::new();
    let mut pos = start.min(data.len());
    while pos < end {
        out.push_str(&format!("{pos:06x}: "));
        for b in &data[pos..end.min(pos + 16)] {
            out.push_str(&format!("{b:02x} "));
        }
        out.push_str(" |");
        for b in &data[pos..end.min(pos + 16)] {
            let ch = if (0x20..=0x7e).contains(b) {
                *b as char
            } else {
                '.'
            };
            out.push(ch);
        }
        out.push_str("|\n");
        pos += 16;
    }
    out
}

fn cmd_assets(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!(
            "usage: qq_analyzer_rs assets <basename-match|c2c-md5-hits|candidate-rules> [options]"
        );
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "basename-match" => {
            let opts = CommonOpts::parse(rest)?;
            let unresolved = opts.unresolved.clone().ok_or_else(|| {
                anyhow::anyhow!("--unresolved <unresolved_image_rows.tsv> is required")
            })?;
            if opts.asset_roots.is_empty() {
                anyhow::bail!("at least one --asset-root <dir> is required");
            }
            let out_dir = opts
                .output
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--out <dir> is required"))?;
            let summary = asset_audit::basename_match(&asset_audit::BasenameMatchOptions {
                unresolved,
                roots: opts.asset_roots,
                out_dir,
            })?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "c2c-md5-hits" => {
            let opts = CommonOpts::parse(rest)?;
            let unresolved = opts.unresolved.clone().ok_or_else(|| {
                anyhow::anyhow!("--unresolved <unresolved_image_rows.tsv> is required")
            })?;
            if opts.asset_roots.is_empty() {
                anyhow::bail!("at least one --asset-root <dir> is required");
            }
            let out = opts
                .output
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--out <c2c_md5_hits.tsv> is required"))?;
            let summary = asset_audit::c2c_md5_hits(&asset_audit::C2cMd5HitsOptions {
                unresolved,
                roots: opts.asset_roots,
                out,
            })?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "candidate-rules" => {
            let opts = CommonOpts::parse(rest)?;
            let audit = opts.input.clone().ok_or_else(|| {
                anyhow::anyhow!("--input <image_asset_candidates.tsv> is required")
            })?;
            if opts.asset_roots.is_empty() {
                anyhow::bail!("at least one --asset-root <dir> is required");
            }
            let summary = asset_audit::candidate_rules(&asset_audit::CandidateRulesOptions {
                audit,
                roots: opts.asset_roots,
            })?;
            write_or_print_json(opts.output.as_ref(), &summary)
        }
        _ => anyhow::bail!("unknown assets subcommand: {subcommand}"),
    }
}

fn cmd_info(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!(
            "usage: qq_analyzer_rs info <label|group-profile|group-members|contact-profiles|stream> [options]"
        );
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    let mut opts = CommonOpts::parse(rest)?;
    let account = opts.resolve_account()?;
    let root = opts
        .input
        .clone()
        .unwrap_or_else(|| config::info_root_current(&opts.root, &account));
    let key_log = opts
        .credential_file
        .clone()
        .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, &account));
    let storage = info_storage::InfoStorage::new(root.clone(), key_log.clone());
    if !storage.available() {
        anyhow::bail!(
            "InfoStorage unavailable; root={} key_log={} account={}",
            root.display(),
            key_log.display(),
            account
        );
    }
    match subcommand.as_str() {
        "label" => {
            let kind = opts
                .info_kind
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--info-kind <buddy|group|discuss> is required"))?;
            let ident = opts
                .ident
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--id <uin-or-group-id> is required"))?;
            let label = storage.label(&kind, &ident)?;
            write_or_print_json(
                opts.output.as_ref(),
                &json!({
                    "account": account,
                    "info_root": root,
                    "kind": kind,
                    "id": ident,
                    "label": label,
                }),
            )
        }
        "group-profile" => {
            let group_id = opts
                .group_id
                .clone()
                .or(opts.ident.clone())
                .ok_or_else(|| anyhow::anyhow!("--group <group-id> is required"))?;
            let profile = storage.group_profile(&group_id)?;
            write_or_print_json(opts.output.as_ref(), &profile)
        }
        "group-members" => {
            let group_id = opts
                .group_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--group <group-id> is required"))?;
            if opts.uins.is_empty() {
                anyhow::bail!("at least one --uin <member-uin> is required");
            }
            let wanted = opts.uins.iter().cloned().collect::<HashSet<_>>();
            let profiles = storage.group_member_profiles_for(&group_id, &wanted)?;
            write_or_print_json(opts.output.as_ref(), &profiles)
        }
        "contact-profiles" => {
            if opts.uins.is_empty() {
                anyhow::bail!("at least one --uin <contact-uin> is required");
            }
            let wanted = opts.uins.iter().cloned().collect::<HashSet<_>>();
            let mut profiles = storage.contact_profiles_for(&wanted)?;
            let friend_social = storage.friend_social_image_profiles_for(&wanted)?;
            for (uin, profile) in friend_social {
                profiles.entry(uin).or_insert(profile);
            }
            write_or_print_json(opts.output.as_ref(), &profiles)
        }
        "stream" => {
            let stream = opts
                .stream
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--stream <relative-info-stream> is required"))?;
            let wanted = if opts.uins.is_empty() {
                None
            } else {
                Some(opts.uins.iter().cloned().collect::<HashSet<_>>())
            };
            let entries = storage.entries_for_stream(&stream, wanted.as_ref())?;
            let json = entries
                .into_iter()
                .map(|(key, fields)| {
                    (
                        key,
                        fields
                            .into_iter()
                            .map(txdata_field_json)
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            write_or_print_json(opts.output.as_ref(), &json)
        }
        _ => anyhow::bail!("unknown info subcommand: {subcommand}"),
    }
}

fn txdata_field_json(
    field: msg3_richtext_parser_rs::txdata_codec::TxDataField,
) -> serde_json::Value {
    json!({
        "header": field.header,
        "name": field.name,
        "value_len": field.value_len,
        "value_hex_prefix": hex_prefix(&field.value, 80),
        "value_texts": field.value_texts,
        "value_text_len": field.value_text_len,
    })
}

fn hex_prefix(data: &[u8], limit: usize) -> String {
    let mut out = data
        .iter()
        .take(limit)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    if data.len() > limit {
        out.push_str(" ...");
    }
    out
}

fn cmd_html(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!("usage: qq_analyzer_rs html <check-links> [options]");
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "check-links" => {
            let opts = CommonOpts::parse(rest)?;
            let root = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <html-output-dir> is required"))?;
            let report = html_check::check_links(&root)?;
            let out = opts
                .output
                .clone()
                .unwrap_or_else(|| html_check::default_report_path(&root));
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&out, serde_json::to_string_pretty(&report)?)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "root": report.root,
                    "html_files": report.html_files,
                    "links_total": report.links_total,
                    "local_links": report.local_links,
                    "alive_local": report.alive_local,
                    "dead_local": report.dead_local,
                    "external_or_data": report.external_or_data,
                    "anchors_only": report.anchors_only,
                    "out": out,
                }))?
            );
            if report.dead_local > 0 {
                anyhow::bail!("dead local links found: {}", report.dead_local);
            }
            Ok(())
        }
        _ => anyhow::bail!("unknown html subcommand: {subcommand}"),
    }
}

fn cmd_db(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!(
            "usage: qq_analyzer_rs db <analyze|sample|inspect|export|sender-rows> [options]"
        );
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "analyze" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let report = db_analysis::analyze_databases(
                &opts.root,
                &account,
                &db_analysis::DbAnalysisOptions {
                    input: opts.input.clone(),
                    out_dir: opts.output.clone(),
                    only: opts.only,
                    db_limit: opts.db_limit,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        "sample" => {
            let opts = CommonOpts::parse(rest)?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <sqlite-db> is required"))?;
            let report = sqlite_tools::sample_sqlite(&input, opts.limit.unwrap_or(5))?;
            write_or_print_json(opts.output.as_ref(), &report)
        }
        "inspect" => {
            let opts = CommonOpts::parse(rest)?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <sqlite-db> is required"))?;
            let report = sqlite_tools::inspect_sqlite(&input)?;
            write_or_print_json(opts.output.as_ref(), &report)
        }
        "export" => {
            let opts = CommonOpts::parse(rest)?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <sqlite-db> is required"))?;
            let out_dir = opts
                .output
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--out <export-dir> is required"))?;
            let report = sqlite_tools::export_sqlite(&input, &out_dir, opts.force)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        "sender-rows" => {
            let opts = CommonOpts::parse(rest)?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <Msg3.0.db> is required"))?;
            let sender = opts
                .sender
                .ok_or_else(|| anyhow::anyhow!("--sender <uin> is required"))?;
            let report = sqlite_tools::find_sender_rows(
                &input,
                sender,
                opts.limit_per_table.unwrap_or(3),
                opts.max_results.unwrap_or(100),
            )?;
            write_or_print_json(opts.output.as_ref(), &report)
        }
        _ => anyhow::bail!("unknown db subcommand: {subcommand}"),
    }
}

fn cmd_catalog(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!("usage: qq_analyzer_rs catalog <summary|source> [options]");
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "summary" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let path = opts
                .input
                .clone()
                .unwrap_or_else(|| catalog::default_catalog_path(&opts.root, &account));
            let summary = catalog::summarize(&path)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "source" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let role = opts
                .role
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--role <catalog-role> is required"))?;
            let path = opts
                .input
                .clone()
                .unwrap_or_else(|| catalog::default_catalog_path(&opts.root, &account));
            let source = catalog::source_by_role(&path, &role)?;
            println!("{}", serde_json::to_string_pretty(&source)?);
            Ok(())
        }
        _ => anyhow::bail!("unknown catalog subcommand: {subcommand}"),
    }
}

fn cmd_capture(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!("usage: qq_analyzer_rs capture <script|normalize-events> [options]");
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "script" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let kind = opts.hook_kind.unwrap_or(capture::HookKind::Infostorage);
            let out = opts.output.unwrap_or_else(|| {
                config::account_output_root(&opts.root, &account)
                    .join("capture")
                    .join(format!("{kind:?}.js").to_ascii_lowercase())
            });
            capture::write_hook_script(kind, &account, &out)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "account": account,
                    "kind": kind,
                    "out": out,
                }))?
            );
            Ok(())
        }
        "normalize-events" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <events.jsonl> is required"))?;
            let out = opts
                .credential_file
                .clone()
                .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, &account));
            let summary = capture::normalize_event_log(&account, &input, &out)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "ensure-frida" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let summary = capture::ensure_frida_inject(&capture::FridaDependencyOptions {
                cache_root: config::account_output_root(&opts.root, &account)
                    .join("deps")
                    .join("frida"),
                version: opts.frida_version.clone(),
                force: opts.force,
            })?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "run" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let kind = opts.hook_kind.unwrap_or(capture::HookKind::Infostorage);
            let summary = run_capture_kind(&opts, &account, kind)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "all" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let pcqq = run_capture_kind(&opts, &account, capture::HookKind::PcqqSqlite)?;
            let infostorage = run_capture_kind(&opts, &account, capture::HookKind::Infostorage)?;
            let credential_path = opts
                .credential_file
                .clone()
                .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, &account));
            let credential_summary = credentials::summarize(&credential_path)?;
            if credential_summary.pcqq_sqlite_keys == 0
                || credential_summary.infostorage_tea_keys == 0
            {
                anyhow::bail!(
                    "capture all did not produce complete credentials: pcqq_sqlite_keys={} infostorage_tea_keys={} credentials={}",
                    credential_summary.pcqq_sqlite_keys,
                    credential_summary.infostorage_tea_keys,
                    credential_path.display()
                );
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "account": account,
                    "credentials": credential_summary,
                    "pcqq_sqlite": pcqq,
                    "infostorage": infostorage,
                }))?
            );
            Ok(())
        }
        "pcqq-rekey" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <copied-pcqq-db> is required"))?;
            let credential_path = opts
                .credential_file
                .clone()
                .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, &account));
            let key_hex = if let Some(key_hex) = opts.key_hex.clone() {
                key_hex
            } else {
                let records = credentials::read_credentials(&credential_path)?;
                credentials::pcqq_sqlite_key_for_path(&records, &input)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no pcqq_sqlite_key credential for {}; pass --key-hex or --credentials",
                            input.display()
                        )
                    })?
                    .key_hex
            };
            let capture_root = config::account_output_root(&opts.root, &account).join("capture");
            let script_path = opts
                .output
                .clone()
                .unwrap_or_else(|| capture_root.join("pcqq_rekey.js"));
            let events_path = opts
                .events
                .clone()
                .unwrap_or_else(|| capture_root.join("pcqq_rekey.events.jsonl"));
            let process = if opts.pid.is_some() {
                opts.process.clone()
            } else {
                opts.process.clone().or_else(|| Some("QQ.exe".to_string()))
            };
            let frida = resolve_frida(&opts, &account)?;
            let summary = capture::run_pcqq_rekey(&capture::PcqqRekeyOptions {
                frida,
                process,
                pid: opts.pid,
                input_path: input,
                key_hex,
                script_path,
                events_path,
                timeout: opts.timeout_seconds.map(Duration::from_secs),
            })?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "pcqq-query" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let input = opts
                .input
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--input <copied-pcqq-db> is required"))?;
            let sql = opts
                .sql
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--sql <select-or-pragma> is required"))?;
            let credential_path = opts
                .credential_file
                .clone()
                .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, &account));
            let key_hex = if let Some(key_hex) = opts.key_hex.clone() {
                key_hex
            } else {
                let records = if credential_path.is_file() {
                    credentials::read_credentials(&credential_path)?
                } else {
                    Vec::new()
                };
                credentials::pcqq_sqlite_key_for_path(&records, &input)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no pcqq_sqlite_key credential matched {}; pass --key-hex",
                            input.display()
                        )
                    })?
                    .key_hex
            };
            let capture_root = config::account_output_root(&opts.root, &account).join("capture");
            let script_path = opts
                .output
                .clone()
                .unwrap_or_else(|| capture_root.join("pcqq-query.js"));
            let events_path = opts
                .events
                .clone()
                .unwrap_or_else(|| capture_root.join("pcqq-query.events.jsonl"));
            let frida = if let Some(frida) = opts.frida {
                frida
            } else if let Ok(frida) = env::var("QQ_ANALYZER_FRIDA") {
                frida
            } else {
                let summary = capture::ensure_frida_inject(&capture::FridaDependencyOptions {
                    cache_root: config::account_output_root(&opts.root, &account)
                        .join("deps")
                        .join("frida"),
                    version: opts.frida_version.clone(),
                    force: false,
                })?;
                summary.executable_path
            };
            let process = if opts.pid.is_some() {
                opts.process
            } else {
                opts.process.or_else(|| Some("QQ.exe".to_string()))
            };
            let summary = capture::run_pcqq_query(&capture::PcqqQueryOptions {
                frida,
                process,
                pid: opts.pid,
                input_path: input,
                key_hex,
                sql,
                max_rows: opts.limit.unwrap_or(100),
                script_path,
                events_path,
                timeout: opts.timeout_seconds.map(Duration::from_secs),
            })?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "pcqq-key-storage-probe" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let capture_root = config::account_output_root(&opts.root, &account).join("capture");
            let script_path = opts
                .output
                .clone()
                .unwrap_or_else(|| capture_root.join("pcqq-key-storage-probe.js"));
            let events_path = opts
                .events
                .clone()
                .unwrap_or_else(|| capture_root.join("pcqq-key-storage-probe.events.jsonl"));
            let frida = if let Some(frida) = opts.frida {
                frida
            } else if let Ok(frida) = env::var("QQ_ANALYZER_FRIDA") {
                frida
            } else {
                let summary = capture::ensure_frida_inject(&capture::FridaDependencyOptions {
                    cache_root: config::account_output_root(&opts.root, &account)
                        .join("deps")
                        .join("frida"),
                    version: opts.frida_version.clone(),
                    force: false,
                })?;
                summary.executable_path
            };
            let process = if opts.pid.is_some() {
                opts.process
            } else {
                opts.process.or_else(|| Some("QQ.exe".to_string()))
            };
            let summary =
                capture::run_pcqq_key_storage_probe(&capture::PcqqKeyStorageProbeOptions {
                    frida,
                    process,
                    pid: opts.pid,
                    account,
                    script_path,
                    events_path,
                    timeout: opts.timeout_seconds.map(Duration::from_secs),
                })?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        _ => anyhow::bail!("unknown capture subcommand: {subcommand}"),
    }
}

fn run_capture_kind(
    opts: &CommonOpts,
    account: &str,
    kind: capture::HookKind,
) -> anyhow::Result<capture::CaptureRunSummary> {
    let capture_root = config::account_output_root(&opts.root, account).join("capture");
    let kind_name = hook_kind_file_stem(kind);
    let script_path = opts
        .output
        .clone()
        .unwrap_or_else(|| capture_root.join(format!("{kind_name}.js")));
    let events_path = opts
        .events
        .clone()
        .unwrap_or_else(|| capture_root.join(format!("{kind_name}.events.jsonl")));
    let credentials_path = if opts.no_import {
        None
    } else {
        Some(
            opts.credential_file
                .clone()
                .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, account)),
        )
    };
    let process = if opts.pid.is_some() || opts.spawn.is_some() {
        opts.process.clone()
    } else {
        opts.process.clone().or_else(|| Some("QQ.exe".to_string()))
    };
    let frida = resolve_frida(opts, account)?;
    capture::run_frida_capture(&capture::CaptureRunOptions {
        kind,
        account: account.to_string(),
        frida,
        process,
        pid: opts.pid,
        spawn: opts.spawn.clone(),
        script_path,
        events_path,
        credentials_path,
        timeout: opts.timeout_seconds.map(Duration::from_secs),
    })
}

fn resolve_frida(opts: &CommonOpts, account: &str) -> anyhow::Result<String> {
    if let Some(frida) = opts.frida.clone() {
        return Ok(frida);
    }
    if let Ok(frida) = env::var("QQ_ANALYZER_FRIDA") {
        if !frida.trim().is_empty() {
            return Ok(frida);
        }
    }
    let summary = capture::ensure_frida_inject(&capture::FridaDependencyOptions {
        cache_root: config::account_output_root(&opts.root, account)
            .join("deps")
            .join("frida"),
        version: opts.frida_version.clone(),
        force: false,
    })?;
    Ok(summary.executable_path)
}

fn hook_kind_file_stem(kind: capture::HookKind) -> &'static str {
    match kind {
        capture::HookKind::Infostorage => "infostorage",
        capture::HookKind::PcqqSqlite => "pcqq-sqlite",
    }
}

fn cmd_inventory(args: Vec<String>) -> anyhow::Result<()> {
    let mut opts = CommonOpts::parse(args)?;
    let account = opts.resolve_account()?;
    let report = inventory::build_inventory(&opts.root, &account);
    let out = config::account_output_root(&opts.root, &account).join("inventory");
    inventory::write_reports(&out, &report)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "account": account,
            "roots": report.roots.len(),
            "databases": report.databases.len(),
            "inventory_dir": out,
            "report": report,
        }))?
    );
    Ok(())
}

fn cmd_credentials(args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcommand) = args.first().cloned() else {
        anyhow::bail!(
            "usage: qq_analyzer_rs credentials <summary|import-key|import-legacy-infostorage> [options]"
        );
    };
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    match subcommand.as_str() {
        "summary" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let path = opts
                .credential_file
                .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, &account));
            let summary = credentials::summarize(&path)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        "import-key" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let kind = opts.credential_kind.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "--credential-kind <pcqq-sqlite|infostorage-tea|ntqq-sqlcipher|other> is required"
                )
            })?;
            let key_hex = opts
                .key_hex
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--key-hex <hex> is required"))?;
            let out = opts
                .credential_file
                .clone()
                .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, &account));
            let db_path = opts
                .input
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            let mut metadata = BTreeMap::new();
            if let Some(input) = opts.input.as_ref() {
                if let Some(basename) = input.file_name().and_then(|name| name.to_str()) {
                    metadata.insert("basename".to_string(), basename.to_string());
                }
            }
            let record = credentials::CredentialRecord {
                kind,
                account: account.clone(),
                source: "manual_rust_cli".to_string(),
                db_path,
                key_hex,
                key_len: 0,
                captured_at: String::new(),
                metadata,
            }
            .normalized();
            credentials::append_credential(&out, record.clone())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "account": account,
                    "out": out,
                    "record": record,
                }))?
            );
            Ok(())
        }
        "import-legacy-infostorage" => {
            let mut opts = CommonOpts::parse(rest)?;
            let account = opts.resolve_account()?;
            let legacy = opts
                .input
                .clone()
                .unwrap_or_else(|| credentials::legacy_infostorage_key_path(&opts.root, &account));
            let out = opts
                .credential_file
                .clone()
                .unwrap_or_else(|| credentials::default_credentials_path(&opts.root, &account));
            let imported = import_legacy_infostorage_keys(&account, &legacy, &out)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "account": account,
                    "legacy": legacy,
                    "out": out,
                    "imported": imported,
                }))?
            );
            Ok(())
        }
        _ => anyhow::bail!("unknown credentials subcommand: {subcommand}"),
    }
}

fn cmd_preprocess(args: Vec<String>) -> anyhow::Result<()> {
    let mut opts = CommonOpts::parse(args)?;
    if opts.rekey_pcqq_dbs && !opts.prepare_pcqq_dbs {
        anyhow::bail!("--rekey-pcqq-dbs requires --prepare-pcqq-dbs");
    }
    let account = opts.resolve_account()?;
    let credential_path = opts
        .credential_file
        .unwrap_or_else(|| preprocess::default_preprocess_credential_path(&opts.root, &account));
    let preprocess_options = preprocess::PreprocessOptions {
        prepare_pcqq_dbs: opts.prepare_pcqq_dbs,
        prepare_ntqq_dbs: opts.prepare_ntqq_dbs,
        rekey_pcqq_dbs: opts.rekey_pcqq_dbs,
        extract_cfb: opts.extract_cfb,
        cfb_stream_limit: opts.cfb_stream_limit,
        force: opts.force,
        db_limit: opts.db_limit,
        only: opts.only,
        frida: opts.frida,
        frida_version: opts.frida_version,
        process: opts.process,
        pid: opts.pid,
        timeout_seconds: opts.timeout_seconds,
    };
    let report = preprocess::run_preprocess_with_options(
        &opts.root,
        &account,
        &credential_path,
        &preprocess_options,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn import_legacy_infostorage_keys(
    account: &str,
    legacy: &PathBuf,
    out: &PathBuf,
) -> anyhow::Result<usize> {
    if !legacy.is_file() {
        anyhow::bail!("legacy key log is missing: {}", legacy.display());
    }
    let mut existing = if out.is_file() {
        credentials::read_credentials(out)?
    } else {
        Vec::new()
    };
    let text = std::fs::read_to_string(legacy)?;
    let mut imported = 0usize;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|err| anyhow::anyhow!("{}:{}: {err}", legacy.display(), index + 1))?;
        let Some(key_hex) = value.get("key_hex").and_then(|v| v.as_str()) else {
            continue;
        };
        let mut metadata = std::collections::BTreeMap::new();
        if let Some(path) = value.get("path").and_then(|v| v.as_str()) {
            metadata.insert("legacy_path".to_string(), path.to_string());
        }
        let record = credentials::CredentialRecord {
            kind: credentials::CredentialKind::InfostorageTeaKey,
            account: account.to_string(),
            source: "legacy_python_infostorage_key_log".to_string(),
            db_path: String::new(),
            key_hex: key_hex.to_string(),
            key_len: 0,
            captured_at: value
                .get("captured_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            metadata,
        };
        let normalized = record.normalized();
        let duplicate = existing.iter().any(|item| {
            item.kind == normalized.kind
                && item.account == normalized.account
                && item.key_hex == normalized.key_hex
                && item.source == normalized.source
        });
        if !duplicate {
            credentials::append_credential(out, normalized.clone())?;
            existing.push(normalized);
            imported += 1;
        }
    }
    Ok(imported)
}

fn write_or_print_json<T: serde::Serialize>(
    output: Option<&PathBuf>,
    value: &T,
) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    if let Some(path) = output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
    } else {
        println!("{text}");
    }
    Ok(())
}

#[derive(Default)]
struct CommonOpts {
    root: PathBuf,
    account: Option<String>,
    credential_file: Option<PathBuf>,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    hook_kind: Option<capture::HookKind>,
    credential_kind: Option<credentials::CredentialKind>,
    prepare_pcqq_dbs: bool,
    prepare_ntqq_dbs: bool,
    rekey_pcqq_dbs: bool,
    extract_cfb: bool,
    cfb_stream_limit: Option<usize>,
    force: bool,
    db_limit: Option<usize>,
    only: Vec<String>,
    role: Option<String>,
    process: Option<String>,
    pid: Option<u32>,
    spawn: Option<String>,
    frida: Option<String>,
    events: Option<PathBuf>,
    timeout_seconds: Option<u64>,
    no_import: bool,
    strict: bool,
    frida_version: Option<String>,
    key_hex: Option<String>,
    limit: Option<usize>,
    limit_per_table: Option<usize>,
    max_results: Option<usize>,
    sender: Option<i64>,
    unresolved: Option<PathBuf>,
    asset_roots: Vec<PathBuf>,
    tables: Vec<String>,
    known_rows: Vec<String>,
    rows_per_table: Option<usize>,
    all_message_tables: bool,
    max_tables: Option<usize>,
    conversation_account: Option<i64>,
    like: Option<String>,
    fts_match: Option<String>,
    info_kind: Option<String>,
    ident: Option<String>,
    group_id: Option<String>,
    uins: Vec<String>,
    stream: Option<String>,
    rowid: Option<i64>,
    start: Option<usize>,
    byte_len: Option<usize>,
    sql: Option<String>,
}

impl CommonOpts {
    fn parse(args: Vec<String>) -> anyhow::Result<Self> {
        let mut opts = CommonOpts {
            root: config::default_root()?,
            ..CommonOpts::default()
        };
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--root" => opts.root = PathBuf::from(iter.next().unwrap_or_default()),
                "--account" => opts.account = Some(iter.next().unwrap_or_default()),
                "--credentials" | "--credential-file" => {
                    opts.credential_file = Some(PathBuf::from(iter.next().unwrap_or_default()))
                }
                "--input" => opts.input = Some(PathBuf::from(iter.next().unwrap_or_default())),
                "--unresolved" => {
                    opts.unresolved = Some(PathBuf::from(iter.next().unwrap_or_default()))
                }
                "--asset-root" => opts
                    .asset_roots
                    .push(PathBuf::from(iter.next().unwrap_or_default())),
                "--out" | "--output" => {
                    opts.output = Some(PathBuf::from(iter.next().unwrap_or_default()))
                }
                "--kind" => {
                    opts.hook_kind =
                        Some(capture::HookKind::parse(&iter.next().unwrap_or_default())?)
                }
                "--credential-kind" => {
                    opts.credential_kind =
                        Some(parse_credential_kind(&iter.next().unwrap_or_default())?)
                }
                "--prepare-pcqq-dbs" => opts.prepare_pcqq_dbs = true,
                "--prepare-ntqq-dbs" => opts.prepare_ntqq_dbs = true,
                "--rekey-pcqq-dbs" => opts.rekey_pcqq_dbs = true,
                "--extract-cfb" => opts.extract_cfb = true,
                "--cfb-stream-limit" => {
                    opts.cfb_stream_limit = Some(iter.next().unwrap_or_default().parse()?)
                }
                "--force" => opts.force = true,
                "--db-limit" => opts.db_limit = Some(iter.next().unwrap_or_default().parse()?),
                "--only" => opts.only.push(iter.next().unwrap_or_default()),
                "--table" => opts.tables.push(iter.next().unwrap_or_default()),
                "--known-row" => opts.known_rows.push(iter.next().unwrap_or_default()),
                "--rows-per-table" => {
                    opts.rows_per_table = Some(iter.next().unwrap_or_default().parse()?)
                }
                "--all-message-tables" => opts.all_message_tables = true,
                "--max-tables" => opts.max_tables = Some(iter.next().unwrap_or_default().parse()?),
                "--role" => opts.role = Some(iter.next().unwrap_or_default()),
                "--process" => opts.process = Some(iter.next().unwrap_or_default()),
                "--pid" => opts.pid = Some(iter.next().unwrap_or_default().parse()?),
                "--spawn" => opts.spawn = Some(iter.next().unwrap_or_default()),
                "--frida" => opts.frida = Some(iter.next().unwrap_or_default()),
                "--events" => opts.events = Some(PathBuf::from(iter.next().unwrap_or_default())),
                "--frida-version" => opts.frida_version = Some(iter.next().unwrap_or_default()),
                "--key-hex" => opts.key_hex = Some(iter.next().unwrap_or_default()),
                "--limit" => opts.limit = Some(iter.next().unwrap_or_default().parse()?),
                "--limit-per-table" => {
                    opts.limit_per_table = Some(iter.next().unwrap_or_default().parse()?)
                }
                "--max-results" => {
                    opts.max_results = Some(iter.next().unwrap_or_default().parse()?)
                }
                "--sender" => opts.sender = Some(iter.next().unwrap_or_default().parse()?),
                "--conversation-account" => {
                    opts.conversation_account = Some(iter.next().unwrap_or_default().parse()?)
                }
                "--like" => opts.like = Some(iter.next().unwrap_or_default()),
                "--match" => opts.fts_match = Some(iter.next().unwrap_or_default()),
                "--info-kind" => opts.info_kind = Some(iter.next().unwrap_or_default()),
                "--id" => opts.ident = Some(iter.next().unwrap_or_default()),
                "--group" => opts.group_id = Some(iter.next().unwrap_or_default()),
                "--uin" => opts.uins.push(iter.next().unwrap_or_default()),
                "--stream" => opts.stream = Some(iter.next().unwrap_or_default()),
                "--rowid" => opts.rowid = Some(iter.next().unwrap_or_default().parse()?),
                "--start" => opts.start = Some(iter.next().unwrap_or_default().parse()?),
                "--len" => opts.byte_len = Some(iter.next().unwrap_or_default().parse()?),
                "--sql" => opts.sql = Some(iter.next().unwrap_or_default()),
                "--timeout-seconds" => {
                    opts.timeout_seconds = Some(iter.next().unwrap_or_default().parse()?)
                }
                "--no-import" => opts.no_import = true,
                "--strict" => opts.strict = true,
                _ => anyhow::bail!("unknown option: {arg}"),
            }
        }
        Ok(opts)
    }

    fn resolve_account(&mut self) -> anyhow::Result<String> {
        config::resolve_account(&self.root, self.account.take())
    }
}

fn parse_credential_kind(value: &str) -> anyhow::Result<credentials::CredentialKind> {
    let normalized = value.trim().replace('_', "-").to_ascii_lowercase();
    match normalized.as_str() {
        "pcqq" | "pcqq-sqlite" | "pcqq-sqlite-key" => {
            Ok(credentials::CredentialKind::PcqqSqliteKey)
        }
        "infostorage" | "info-storage" | "infostorage-tea" | "infostorage-tea-key" => {
            Ok(credentials::CredentialKind::InfostorageTeaKey)
        }
        "ntqq" | "sqlcipher" | "ntqq-sqlcipher" | "ntqq-sqlcipher-key" => {
            Ok(credentials::CredentialKind::NtqqSqlcipherKey)
        }
        "other" => Ok(credentials::CredentialKind::Other),
        _ => anyhow::bail!(
            "unknown credential kind: {value}; expected pcqq-sqlite, infostorage-tea, ntqq-sqlcipher, or other"
        ),
    }
}

fn print_usage() {
    println!(
        "usage:
  qq_analyzer_rs assets basename-match --unresolved <unresolved_image_rows.tsv> --asset-root <dir> [--asset-root <dir> ...] --out <dir>
  qq_analyzer_rs assets c2c-md5-hits --unresolved <unresolved_image_rows.tsv> --asset-root <dir> [--asset-root <dir> ...] --out <c2c_md5_hits.tsv>
  qq_analyzer_rs assets candidate-rules --input <image_asset_candidates.tsv> --asset-root <dir> [--asset-root <dir> ...] [--out <report.json>]
  qq_analyzer_rs inventory [--root <workspace>] [--account <uin>]
  qq_analyzer_rs capture script [--root <workspace>] [--account <uin>] [--kind <infostorage|pcqq-sqlite>] [--out <js>]
  qq_analyzer_rs capture ensure-frida [--root <workspace>] [--account <uin>] [--frida-version <version|latest>] [--force]
  qq_analyzer_rs capture all [--root <workspace>] [--account <uin>] [--process <name>|--pid <pid>] [--frida <path>] [--frida-version <version|latest>] [--credentials <jsonl>] [--timeout-seconds <n>]
  qq_analyzer_rs capture run [--root <workspace>] [--account <uin>] [--kind <infostorage|pcqq-sqlite>] [--process <name>|--pid <pid>] [--frida <path>] [--frida-version <version|latest>] [--out <js>] [--events <jsonl>] [--credentials <jsonl>] [--timeout-seconds <n>] [--no-import]
  qq_analyzer_rs capture pcqq-rekey [--root <workspace>] [--account <uin>] --input <copied-pcqq-db> [--key-hex <hex>|--credentials <jsonl>] [--process <name>|--pid <pid>] [--frida <path>] [--frida-version <version|latest>] [--out <js>] [--events <jsonl>] [--timeout-seconds <n>]
  qq_analyzer_rs capture pcqq-key-storage-probe [--root <workspace>] [--account <uin>] [--process <name>|--pid <pid>] [--frida <path>] [--frida-version <version|latest>] [--out <js>] [--events <jsonl>] [--timeout-seconds <n>]
  qq_analyzer_rs capture normalize-events [--root <workspace>] [--account <uin>] --input <events.jsonl> [--credentials <jsonl>]
  qq_analyzer_rs catalog summary [--root <workspace>] [--account <uin>] [--input <catalog.sqlite>]
  qq_analyzer_rs catalog source [--root <workspace>] [--account <uin>] [--input <catalog.sqlite>] --role <catalog-role>
  qq_analyzer_rs credentials summary [--root <workspace>] [--account <uin>] [--credentials <jsonl>]
  qq_analyzer_rs credentials import-key [--root <workspace>] [--account <uin>] --credential-kind <pcqq-sqlite|infostorage-tea|ntqq-sqlcipher|other> --key-hex <hex> [--input <db-path>] [--credentials <jsonl>]
  qq_analyzer_rs credentials import-legacy-infostorage [--root <workspace>] [--account <uin>] [--input <legacy-jsonl>] [--credentials <jsonl>]
  qq_analyzer_rs db analyze [--root <workspace>] [--account <uin>] [--input <db-or-dir>] [--out <analysis-dir>] [--only <selector>] [--db-limit <n>]
  qq_analyzer_rs db sample --input <sqlite-db> [--limit <n>] [--out <report.json>]
  qq_analyzer_rs db inspect --input <sqlite-db> [--out <report.json>]
  qq_analyzer_rs db export --input <sqlite-db> --out <export-dir> [--force]
  qq_analyzer_rs db sender-rows --input <Msg3.0.db> --sender <uin> [--limit-per-table <n>] [--max-results <n>] [--out <report.json>]
  qq_analyzer_rs html check-links --input <html-output-dir> [--out <report.json>]
  qq_analyzer_rs info label [--root <workspace>] [--account <uin>] [--input <Info.db-root>] [--credentials <credentials.jsonl>] --info-kind <buddy|group|discuss> --id <uin-or-group-id> [--out <report.json>]
  qq_analyzer_rs info group-profile [--root <workspace>] [--account <uin>] [--input <Info.db-root>] [--credentials <credentials.jsonl>] --group <group-id> [--out <report.json>]
  qq_analyzer_rs info group-members [--root <workspace>] [--account <uin>] [--input <Info.db-root>] [--credentials <credentials.jsonl>] --group <group-id> --uin <member-uin> [--uin <member-uin> ...] [--out <report.json>]
  qq_analyzer_rs info contact-profiles [--root <workspace>] [--account <uin>] [--input <Info.db-root>] [--credentials <credentials.jsonl>] --uin <contact-uin> [--uin <contact-uin> ...] [--out <report.json>]
  qq_analyzer_rs info stream [--root <workspace>] [--account <uin>] [--input <Info.db-root>] [--credentials <credentials.jsonl>] --stream <relative-info-stream> [--uin <entry-name> ...] [--out <report.json>]
  qq_analyzer_rs migration audit-python [--root <workspace>] [--out <report.json>] [--strict]
  qq_analyzer_rs msg3 row-parse [--root <workspace>] [--account <uin>] [--input <Msg3.0.db>] --table <msg3-table> --rowid <rowid> [--out <report.json>]
  qq_analyzer_rs msg3 row-probe [--root <workspace>] [--account <uin>] [--input <Msg3.0.db>] --table <msg3-table> --rowid <rowid> [--start <byte-offset>] [--len <bytes>] [--out <report.json>]
  qq_analyzer_rs msg3 info-parse --input <info.bin> [--out <report.json>]
  qq_analyzer_rs msg3 index-query --input <Msg3.0index.db> [--conversation-account <uin>] [--like <pattern>] [--match <fts-query>] [--limit <n>] [--out <report.json>]
  qq_analyzer_rs msg3 export-samples --input <Msg3.0.db> --out <samples.tsv> [--table <msg-table>] [--known-row <table:rowid>] [--rows-per-table <n>] [--all-message-tables] [--max-tables <n>]
  qq_analyzer_rs preprocess [--root <workspace>] [--account <uin>] [--credentials <jsonl>] [--prepare-pcqq-dbs] [--prepare-ntqq-dbs] [--rekey-pcqq-dbs] [--process <name>|--pid <pid>] [--frida <path>] [--frida-version <version|latest>] [--timeout-seconds <n>] [--extract-cfb] [--cfb-stream-limit <n>] [--only <db-or-account-relative-path>] [--db-limit <n>] [--force]
  qq_analyzer_rs snapshot test-clone --input <src> --out <dst> [--force]
  qq_analyzer_rs serve [--root <workspace>] [--account <uin>] [--host <addr>] [--port <port>]"
    );
}

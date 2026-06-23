use std::env;
use std::path::{Path, PathBuf};

use crate::{catalog, config, credentials};

#[derive(Clone)]
pub(crate) struct Config {
    pub(crate) root: PathBuf,
    pub(crate) account: String,
    pub(crate) host: String,
    pub(crate) catalog: PathBuf,
    pub(crate) db: PathBuf,
    pub(crate) db_source: String,
    pub(crate) info_root: PathBuf,
    pub(crate) info_root_source: String,
    pub(crate) friend_social_root: PathBuf,
    pub(crate) info_key_log: PathBuf,
    pub(crate) port: u16,
}

pub(crate) fn parse_args() -> anyhow::Result<Config> {
    let mut port = 8765u16;
    let mut host = env::var("QQ_ANALYZER_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let mut root = config::default_root()?;
    let mut account = None::<String>;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => {
                host = args.next().unwrap_or_else(|| "127.0.0.1".to_string());
            }
            "--port" => {
                port = args.next().unwrap_or_else(|| "8765".to_string()).parse()?;
            }
            "--root" => {
                root = PathBuf::from(args.next().unwrap_or_default());
            }
            "--account" => {
                account = Some(args.next().unwrap_or_default());
            }
            _ => {}
        }
    }
    let account = config::resolve_account(&root, account)?;
    let output_root = config::account_output_root(&root, &account);
    let catalog = catalog::default_catalog_path(&root, &account);
    let (db, db_source) = catalog_effective_path(
        &catalog,
        &["msg3_db_prepared", "register_msg3_db"],
        config::msg3_db(&root, &account),
    )?;
    let (info_root, info_root_source) = catalog_effective_path(
        &catalog,
        &["register_infostorage"],
        config::info_root_current(&root, &account),
    )?;
    let friend_social_root = preferred_existing_path([
        config::prepared_cfb_root(&root, &account)
            .join("FriendSocial.db")
            .join("FriendSocial"),
        output_root
            .join("extracted-cfb")
            .join("FriendSocial.db")
            .join("FriendSocial"),
    ]);
    let info_key_log = credentials::default_credentials_path(&root, &account);
    Ok(Config {
        root,
        account,
        host,
        catalog,
        db,
        db_source,
        info_root,
        info_root_source,
        friend_social_root,
        info_key_log,
        port,
    })
}

fn preferred_existing_path<const N: usize>(paths: [PathBuf; N]) -> PathBuf {
    paths
        .iter()
        .find(|path| path.exists())
        .cloned()
        .unwrap_or_else(|| paths[0].clone())
}

fn catalog_effective_path(
    catalog_path: &Path,
    roles: &[&str],
    fallback: PathBuf,
) -> anyhow::Result<(PathBuf, String)> {
    for role in roles {
        if let Some(source) = catalog::source_by_role(catalog_path, role)? {
            if !source.effective_path.is_empty() {
                let path = PathBuf::from(&source.effective_path);
                if path.exists() {
                    return Ok((
                        path,
                        format!(
                            "catalog:{role}:{}",
                            if source.prepared_exists {
                                "prepared"
                            } else {
                                "source"
                            }
                        ),
                    ));
                }
            }
        }
    }
    Ok((fallback, "config_fallback".to_string()))
}

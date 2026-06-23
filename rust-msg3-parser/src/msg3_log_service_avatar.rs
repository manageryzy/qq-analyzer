use std::path::{Path, PathBuf};

use crate::info_storage;
use crate::msg3_log_service_asset_http::asset_href;
use crate::qq_hash;

fn cfb_roots(root: &Path, account: &str) -> Vec<PathBuf> {
    let output = root.join("qq-analyzer").join("output").join(account);
    vec![
        output.join("prepared").join("pcqq").join("cfb"),
        output.join("extracted-cfb"),
        output.join("extracted-cfb-current"),
    ]
}

pub(crate) fn resolve_member_avatar(
    root: &Path,
    account: &str,
    sender_uin: &str,
    member_profile: &info_storage::GroupMemberProfile,
    contact_profile: &info_storage::ContactProfile,
) -> (String, Vec<String>, String) {
    let mut candidates = Vec::new();
    let face_id = member_profile
        .faceshop_face_id
        .or(contact_profile.faceshop_face_id);
    if let Some(face_id) = face_id {
        let base = root.join(account).join("VasscUpdate").join("VasHead");
        for suffix in ["png", "jpg", "jpeg", "webp"] {
            candidates.push(base.join(format!("face.{face_id}.large.{suffix}")));
        }
        candidates.push(base.join(format!("face.{face_id}.large")));
    }
    if !contact_profile.custom_head_file.is_empty() {
        candidates.extend(custom_head_candidates(
            root,
            account,
            &contact_profile.custom_head_file,
        ));
    }
    let avatar_url = candidates
        .iter()
        .find(|path| path.is_file())
        .map(|path| asset_href(path))
        .unwrap_or_default();
    let candidate_texts = candidates
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    if !avatar_url.is_empty() {
        return (
            avatar_url,
            candidate_texts,
            "matched_local_file".to_string(),
        );
    }
    if is_normal_uin(sender_uin) {
        return (
            format!("https://q1.qlogo.cn/g?b=qq&nk={sender_uin}&s=100"),
            candidate_texts,
            "no local custom head index decoded; using qlogo uin fallback".to_string(),
        );
    }
    let reason = if member_profile.faceshop_face_id.is_none()
        && contact_profile.faceshop_face_id.is_none()
        && contact_profile.custom_head_file.is_empty()
    {
        "no dwFaceshopFaceId or strCustomHeadFile* in decoded group/contact TXData".to_string()
    } else {
        "avatar fields decoded but no local candidate file exists".to_string()
    };
    (String::new(), candidate_texts, reason)
}

fn is_normal_uin(value: &str) -> bool {
    value.chars().all(|ch| ch.is_ascii_digit()) && !matches!(value, "" | "0" | "10000" | "1000000")
}

pub(crate) fn resolve_group_avatar(
    root: &Path,
    account: &str,
    profile: &info_storage::GroupProfile,
) -> (String, Vec<String>, String) {
    let mut candidates = Vec::new();
    let mut hash_name = String::new();
    if let Some(hash) = qq_hash::decode_hex_16(&profile.file_hash_hex) {
        if let Some(encoded) = qq_hash::encode_hash(&hash) {
            hash_name = encoded;
        }
    }
    if !profile.image_file_path.is_empty() {
        candidates.extend(custom_head_candidates(
            root,
            account,
            &profile.image_file_path,
        ));
        if let Some(name) = path_basename(&profile.image_file_path) {
            push_group_head_locations(root, account, &name, &mut candidates);
        }
    }
    for path in &profile.head_history_paths {
        candidates.extend(custom_head_candidates(root, account, path));
        if let Some(name) = path_basename(path) {
            push_group_head_locations(root, account, &name, &mut candidates);
        }
    }
    if !hash_name.is_empty() {
        let mut hash_names = vec![hash_name.clone()];
        if let Some(ext) = path_extension(&profile.image_file_path) {
            hash_names.push(format!("{hash_name}.{ext}"));
        }
        for ext in ["bmp", "gif", "jpg", "png"] {
            hash_names.push(format!("{hash_name}.{ext}"));
        }
        hash_names.sort();
        hash_names.dedup();
        for name in hash_names {
            push_group_head_locations(root, account, &name, &mut candidates);
            candidates.push(root.join(account).join("GroupCustomHeadImage").join(&name));
            candidates.push(
                root.join(account)
                    .join("Misc")
                    .join("GroupCustomHeadImage")
                    .join(&name),
            );
        }
    }
    if !profile.group_id.is_empty() {
        candidates.push(
            root.join(account)
                .join("SelCreateGroupHead")
                .join(format!("{}g", profile.group_id)),
        );
    }
    let avatar_url = candidates
        .iter()
        .find(|path| path.is_file())
        .map(|path| asset_href(path))
        .unwrap_or_default();
    let candidate_texts = candidates
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    let reason = if !avatar_url.is_empty() {
        "matched_local_file".to_string()
    } else if profile.image_file_path.is_empty()
        && profile.head_history_paths.is_empty()
        && profile.group_face.unwrap_or(0) == 0
    {
        "no group avatar path or wGroupFace in Group/Basic.db".to_string()
    } else {
        "group avatar fields decoded but no local candidate file exists".to_string()
    };
    (avatar_url, candidate_texts, reason)
}

fn push_group_head_locations(root: &Path, account: &str, name: &str, out: &mut Vec<PathBuf>) {
    for cfb_root in cfb_roots(root, account) {
        out.push(
            cfb_root
                .join("Misc.db")
                .join("GroupCustomHeadImage")
                .join(name),
        );
        out.push(cfb_root.join("MiscHead.db").join(name));
        out.push(
            cfb_root
                .join("MiscHead.db")
                .join("CustomHeadImage")
                .join(name),
        );
    }
}

fn custom_head_candidates(root: &Path, account: &str, value: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let trimmed = value.trim().replace('\\', "/");
    if trimmed.is_empty() {
        return out;
    }
    if let Some(rest) = trimmed.strip_prefix("UserDataMisc:") {
        out.push(root.join(account).join(rest));
        out.push(root.join(account).join("Misc").join(rest));
        out.push(root.join(account).join("MiscHead").join(rest));
        if let Some(name) = path_basename(rest) {
            for cfb_root in cfb_roots(root, account) {
                out.push(
                    cfb_root
                        .join("Misc.db")
                        .join("GroupCustomHeadImage")
                        .join(&name),
                );
            }
            out.push(root.join(account).join("GroupCustomHeadImage").join(&name));
            out.push(root.join(account).join("CustomHeadImage").join(&name));
        }
    } else if let Some(rest) = trimmed.strip_prefix("UserDataRoot:") {
        out.push(root.join(account).join(rest));
    } else if let Some(rest) = trimmed.strip_prefix("OSRoot:") {
        if let Some(name) = path_basename(rest) {
            out.push(
                root.join(account)
                    .join("FriendSocialCustomHeadImage")
                    .join(&name),
            );
            for cfb_root in cfb_roots(root, account) {
                out.push(
                    cfb_root
                        .join("FriendSocial.db")
                        .join("FriendSocial")
                        .join("FriendSocialCustomHeadImage")
                        .join(&name),
                );
            }
        }
    } else if trimmed.contains(':') {
        if let Some((_, rest)) = trimmed.split_once(':') {
            out.push(root.join(account).join(rest));
            if let Some(name) = path_basename(rest) {
                out.push(
                    root.join(account)
                        .join("FriendSocialCustomHeadImage")
                        .join(&name),
                );
            }
        }
    } else {
        out.push(root.join(account).join(&trimmed));
        out.push(root.join(account).join("MiscHead").join(&trimmed));
    }
    out
}

fn path_basename(value: &str) -> Option<String> {
    let normalized = value.trim().replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
}

fn path_extension(value: &str) -> Option<String> {
    path_basename(value).and_then(|name| {
        name.rsplit_once('.')
            .map(|(_, ext)| ext.trim().to_ascii_lowercase())
            .filter(|ext| !ext.is_empty())
    })
}

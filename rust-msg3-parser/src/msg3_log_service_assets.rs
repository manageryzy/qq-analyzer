use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::msg3_log_service_asset_candidates::{
    asset_index_files, collect_index_paths, local_asset_candidates, push_unique_path,
    thumbnail_asset_candidates,
};
use crate::msg3_log_service_asset_http::asset_href;

pub(crate) struct AssetMatch {
    pub(crate) rich_nodes: Value,
    pub(crate) assets: Vec<Value>,
    pub(crate) media_kind: String,
    pub(crate) media_label: String,
    pub(crate) candidate_paths: Vec<String>,
    pub(crate) candidate_count: usize,
    pub(crate) hit_count: usize,
    pub(crate) unmatched_reason: String,
}

pub(crate) fn match_assets_with_resolver(
    resolver: &mut AssetResolver<'_>,
    rich_nodes: &Value,
) -> AssetMatch {
    let mut enriched_nodes = rich_nodes.clone();
    let mut summary = AssetSummary::default();
    resolver.resolve_rich_nodes(&mut enriched_nodes, &mut summary);
    let hit_count = summary.assets.len();
    let unmatched_reason = summary.unmatched_reason();
    AssetMatch {
        rich_nodes: enriched_nodes,
        assets: summary.assets,
        media_kind: summary.media_kind,
        media_label: summary.media_label,
        candidate_paths: summary
            .candidate_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect(),
        candidate_count: summary.candidate_paths.len(),
        hit_count,
        unmatched_reason,
    }
}

/// Produce the same ordered candidate groups as the interactive resolver but
/// without probing the filesystem or cloning/annotating the rich-node tree.
/// The provenance linker can test these candidates against the manifest's
/// indexed path column, which is substantially cheaper at archive scale.
pub(crate) fn candidate_path_groups_with_resolver(
    resolver: &mut AssetResolver<'_>,
    rich_nodes: &Value,
) -> Vec<Vec<String>> {
    fn collect(resolver: &mut AssetResolver<'_>, value: &Value, out: &mut Vec<Vec<String>>) {
        if let Some(nodes) = value.as_array() {
            for node in nodes {
                collect(resolver, node, out);
            }
            return;
        }
        let Some(node) = value.as_object() else {
            return;
        };
        let typ = node.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(typ, "image" | "file" | "video" | "voice" | "face") {
            let mut tokens = asset_tokens(value);
            if typ == "face" {
                if let Some(text) = node.get("text").and_then(Value::as_str) {
                    push_unique_string(&mut tokens, text.trim().to_string());
                }
            }
            let candidates = resolver
                .candidate_paths(typ, &tokens)
                .into_iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            if !candidates.is_empty() {
                out.push(candidates);
            }
        }
        for key in ["children", "items", "items_expanded", "nodes", "rich_nodes"] {
            if let Some(child) = node.get(key) {
                collect(resolver, child, out);
            }
        }
    }

    let mut groups = Vec::new();
    collect(resolver, rich_nodes, &mut groups);
    groups
}

#[derive(Default)]
struct AssetSummary {
    assets: Vec<Value>,
    media_kind: String,
    media_label: String,
    candidate_paths: Vec<PathBuf>,
}

impl AssetSummary {
    fn observe(&mut self, resolved: &ResolvedAsset) {
        if self.media_kind.is_empty() {
            self.media_kind = resolved.kind.clone();
            self.media_label = media_label(&resolved.kind).to_string();
        }
        for path in &resolved.candidates {
            push_unique_path(&mut self.candidate_paths, path.clone());
        }
        for asset in &resolved.assets {
            self.assets.push(asset.clone());
        }
    }

    fn unmatched_reason(&self) -> String {
        if self.media_kind.is_empty() || !self.assets.is_empty() {
            String::new()
        } else if self.candidate_paths.is_empty() {
            "协议中未提取到可直接检查的本地候选路径".to_string()
        } else {
            format!(
                "按协议候选路径和 FileIndex/Resume/缩略图索引即时检查未命中：0/{}",
                self.candidate_paths.len()
            )
        }
    }
}

#[derive(Clone)]
struct ResolvedAsset {
    kind: String,
    tokens: Vec<String>,
    candidates: Vec<PathBuf>,
    assets: Vec<Value>,
}

impl ResolvedAsset {
    fn reason(&self) -> String {
        if self.kind.is_empty() || !self.assets.is_empty() {
            String::new()
        } else if self.tokens.is_empty() {
            "媒体节点没有可解析的协议引用".to_string()
        } else if self.candidates.is_empty() {
            "协议引用没有生成可检查的本地候选路径".to_string()
        } else {
            format!("候选路径即时检查未命中：0/{}", self.candidates.len())
        }
    }
}

pub(crate) struct AssetResolver<'a> {
    root: &'a Path,
    account: &'a str,
    index_data: Option<Vec<Vec<u8>>>,
    resolved: HashMap<String, ResolvedAsset>,
    candidate_paths: HashMap<String, Vec<PathBuf>>,
}

const RESOLVED_ASSET_CACHE_CAPACITY: usize = 100_000;

impl<'a> AssetResolver<'a> {
    pub(crate) fn new(root: &'a Path, account: &'a str) -> Self {
        Self {
            root,
            account,
            index_data: None,
            resolved: HashMap::new(),
            candidate_paths: HashMap::new(),
        }
    }

    fn resolve_rich_nodes(&mut self, rich_nodes: &mut Value, summary: &mut AssetSummary) {
        if let Some(nodes) = rich_nodes.as_array_mut() {
            self.resolve_node_array(nodes, summary);
        }
    }

    fn resolve_node_array(&mut self, nodes: &mut [Value], summary: &mut AssetSummary) {
        for node in nodes {
            self.resolve_node(node, summary);
        }
    }

    fn resolve_node(&mut self, node: &mut Value, summary: &mut AssetSummary) {
        if !node.is_object() {
            return;
        }
        let typ = node["type"].as_str().unwrap_or("").to_string();
        if matches!(typ.as_str(), "image" | "file" | "video" | "voice" | "face") {
            let mut tokens = asset_tokens(node);
            if typ == "face" {
                if let Some(text) = node["text"].as_str() {
                    push_unique_string(&mut tokens, text.trim().to_string());
                }
            }
            let resolved = self.resolve_tokens(&typ, tokens);
            annotate_asset_node(node, &resolved);
            summary.observe(&resolved);
        }
        for key in ["children", "items", "items_expanded", "nodes", "rich_nodes"] {
            if let Some(children) = node[key].as_array_mut() {
                self.resolve_node_array(children, summary);
            }
        }
    }

    fn resolve_tokens(&mut self, kind: &str, tokens: Vec<String>) -> ResolvedAsset {
        let cache_key = format!("{kind}\0{}", tokens.join("\0"));
        if let Some(resolved) = self.resolved.get(&cache_key) {
            return resolved.clone();
        }
        let candidates = self.candidate_paths(kind, &tokens);
        // Candidate lists include many legacy thumbnail/fix suffix variants.
        // On mounted Windows volumes, probing every missing variant can cost
        // hundreds of milliseconds. They are priority ordered, and the UI
        // consumes one resolved asset, so stop at the first existing file.
        let assets = candidates
            .iter()
            .find(|path| path.is_file())
            .map(|path| vec![asset_json(kind, path)])
            .unwrap_or_default();
        let resolved = ResolvedAsset {
            kind: kind.to_string(),
            tokens,
            candidates,
            assets,
        };
        if self.resolved.len() >= RESOLVED_ASSET_CACHE_CAPACITY {
            self.resolved.clear();
        }
        self.resolved.insert(cache_key, resolved.clone());
        resolved
    }

    fn candidate_paths(&mut self, kind: &str, tokens: &[String]) -> Vec<PathBuf> {
        let cache_key = format!("{kind}\0{}", tokens.join("\0"));
        if let Some(candidates) = self.candidate_paths.get(&cache_key) {
            return candidates.clone();
        }
        let mut candidates = Vec::new();
        for token in tokens {
            for path in local_asset_candidates(self.root, self.account, token) {
                push_unique_path(&mut candidates, path);
            }
            for path in thumbnail_asset_candidates(self.root, self.account, token) {
                push_unique_path(&mut candidates, path);
            }
        }
        for path in self.file_index_asset_candidates(tokens) {
            push_unique_path(&mut candidates, path);
        }
        if self.candidate_paths.len() >= RESOLVED_ASSET_CACHE_CAPACITY {
            self.candidate_paths.clear();
        }
        self.candidate_paths.insert(cache_key, candidates.clone());
        candidates
    }

    fn file_index_asset_candidates(&mut self, tokens: &[String]) -> Vec<PathBuf> {
        if tokens.is_empty() {
            return Vec::new();
        }
        if self.index_data.is_none() {
            let mut loaded = Vec::new();
            for file in asset_index_files(self.root, self.account) {
                if let Ok(data) = fs::read(&file) {
                    loaded.push(data);
                }
            }
            self.index_data = Some(loaded);
        }
        let mut out = Vec::new();
        if let Some(items) = &self.index_data {
            for data in items {
                collect_index_paths(self.root, self.account, data, tokens, &mut out);
            }
        }
        out
    }
}

fn asset_tokens(node: &Value) -> Vec<String> {
    let mut tokens = Vec::new();
    for cand in node["candidates"].as_array().into_iter().flatten() {
        if let Some(text) = cand.as_str() {
            push_unique_string(&mut tokens, text.trim().to_string());
        }
    }
    tokens
}

fn annotate_asset_node(node: &mut Value, resolved: &ResolvedAsset) {
    let candidate_paths: Vec<Value> = resolved
        .candidates
        .iter()
        .map(|p| Value::String(p.to_string_lossy().to_string()))
        .collect();
    let reason = resolved.reason();
    if let Some(obj) = node.as_object_mut() {
        obj.insert("assets".to_string(), Value::Array(resolved.assets.clone()));
        obj.insert(
            "asset".to_string(),
            resolved.assets.first().cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "asset_candidates".to_string(),
            Value::Array(candidate_paths),
        );
        obj.insert(
            "asset_candidate_count".to_string(),
            json!(resolved.candidates.len()),
        );
        obj.insert("asset_hit_count".to_string(), json!(resolved.assets.len()));
        obj.insert("asset_reason".to_string(), Value::String(reason));
    }
}

fn asset_json(kind: &str, path: &Path) -> Value {
    json!({
        "kind": kind,
        "path": path.to_string_lossy(),
        "href": asset_href(path),
        "name": path.file_name().and_then(|s| s.to_str()).unwrap_or("")
    })
}

fn media_label(kind: &str) -> &'static str {
    match kind {
        "image" => "图片",
        "face" => "表情",
        "video" => "视频",
        "voice" => "语音",
        _ => "文件",
    }
}

fn push_unique_string(out: &mut Vec<String>, value: String) {
    if !value.is_empty() && !out.iter().any(|v| v == &value) {
        out.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_forward_expansion_contributes_image_candidates() {
        let root = std::env::temp_dir().join("qq-analyzer-nested-forward");
        let mut resolver = AssetResolver::new(&root, "10001");
        let nodes = json!([{
            "type": "multi_msg",
            "items_expanded": [{
                "type": "mmp_item",
                "rich_nodes": [{
                    "type": "multi_msg",
                    "items_expanded": [{
                        "type": "mmp_item",
                        "rich_nodes": [{
                            "type": "image",
                            "candidates": ["UserDataImage:Group\\nested.jpg"]
                        }]
                    }]
                }]
            }]
        }]);
        let groups = candidate_path_groups_with_resolver(&mut resolver, &nodes);
        let expected = root
            .join("10001")
            .join("Image")
            .join("Group")
            .join("nested.jpg")
            .to_string_lossy()
            .to_string();
        assert!(groups
            .iter()
            .any(|candidates| candidates.iter().any(|path| path == &expected)));
    }
}

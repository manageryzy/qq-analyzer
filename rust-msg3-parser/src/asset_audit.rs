use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Debug)]
pub struct BasenameMatchOptions {
    pub unresolved: PathBuf,
    pub roots: Vec<PathBuf>,
    pub out_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct C2cMd5HitsOptions {
    pub unresolved: PathBuf,
    pub roots: Vec<PathBuf>,
    pub out: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CandidateRulesOptions {
    pub audit: PathBuf,
    pub roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BasenameMatchSummary {
    pub wanted_names: usize,
    pub hit_names: usize,
    pub unique_hit_names: usize,
    pub ambiguous_hit_names: usize,
    pub mapped_image_paths: usize,
    pub out_dir: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct C2cMd5HitsSummary {
    pub wanted: usize,
    pub hashed: usize,
    pub hits: usize,
    pub out: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CandidateRulesSummary {
    pub unresolved_images: usize,
    pub rule_hits: BTreeMap<String, usize>,
    pub examples: BTreeMap<String, Vec<CandidateRuleExample>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CandidateRuleExample {
    pub image_path: String,
    pub candidate: String,
}

pub fn basename_match(options: &BasenameMatchOptions) -> anyhow::Result<BasenameMatchSummary> {
    let wanted = wanted_names(&options.unresolved)?;
    let mut hits = wanted
        .keys()
        .map(|name| (name.clone(), BTreeSet::<String>::new()))
        .collect::<BTreeMap<_, _>>();
    for root in &options.roots {
        if root.exists() {
            scan_root_for_names(root, &mut hits)?;
        }
    }

    fs::create_dir_all(&options.out_dir)?;
    let mut map_rows = Vec::new();
    let mut all_rows = Vec::new();
    let mut hit_names = 0usize;
    let mut unique_hit_names = 0usize;
    let mut ambiguous_hit_names = 0usize;
    let mut mapped_image_paths = BTreeSet::new();

    for (name, paths) in &hits {
        if paths.is_empty() {
            continue;
        }
        hit_names += 1;
        let wanted_paths = wanted.get(name).cloned().unwrap_or_default();
        if paths.len() == 1 {
            unique_hit_names += 1;
            let candidate = paths.iter().next().cloned().unwrap_or_default();
            for image_path in &wanted_paths {
                map_rows.push(vec![
                    image_path.clone(),
                    candidate.clone(),
                    "target_basename_unique".to_string(),
                ]);
                mapped_image_paths.insert(image_path.clone());
            }
        } else {
            ambiguous_hit_names += 1;
        }
        for candidate in paths {
            for image_path in &wanted_paths {
                all_rows.push(vec![
                    image_path.clone(),
                    name.clone(),
                    candidate.clone(),
                    if paths.len() > 1 { "1" } else { "0" }.to_string(),
                ]);
            }
        }
    }

    write_tsv(
        &options.out_dir.join("target_basename_map.tsv"),
        &["image_path", "candidate", "rule"],
        &map_rows,
    )?;
    write_tsv(
        &options.out_dir.join("target_basename_hits.tsv"),
        &["image_path", "name", "candidate", "ambiguous"],
        &all_rows,
    )?;
    let summary = BasenameMatchSummary {
        wanted_names: wanted.len(),
        hit_names,
        unique_hit_names,
        ambiguous_hit_names,
        mapped_image_paths: mapped_image_paths.len(),
        out_dir: options.out_dir.display().to_string(),
    };
    fs::write(
        options.out_dir.join("target_basename_summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    Ok(summary)
}

pub fn c2c_md5_hits(options: &C2cMd5HitsOptions) -> anyhow::Result<C2cMd5HitsSummary> {
    let wanted = wanted_c2c_md5_stems(&options.unresolved)?;
    let mut hits = Vec::<(String, String)>::new();
    let mut hashed = 0usize;
    for root in &options.roots {
        if root.exists() {
            scan_root_for_md5(root, &wanted, &mut hashed, &mut hits)?;
        }
    }
    if let Some(parent) = options.out.parent() {
        fs::create_dir_all(parent)?;
    }
    write_tsv(
        &options.out,
        &["md5", "path"],
        &hits
            .iter()
            .map(|(digest, path)| vec![digest.clone(), path.clone()])
            .collect::<Vec<_>>(),
    )?;
    Ok(C2cMd5HitsSummary {
        wanted: wanted.len(),
        hashed,
        hits: hits.len(),
        out: options.out.display().to_string(),
    })
}

pub fn candidate_rules(options: &CandidateRulesOptions) -> anyhow::Result<CandidateRulesSummary> {
    let unresolved = unresolved_image_paths_from_audit(&options.audit)?;
    let mut names = BTreeMap::<String, Vec<String>>::new();
    for root in &options.roots {
        if root.exists() {
            scan_immediate_file_names(root, &mut names)?;
        }
    }
    let mut rule_hits = BTreeMap::<String, usize>::new();
    let mut examples = BTreeMap::<String, Vec<CandidateRuleExample>>::new();
    for image_path in &unresolved {
        for (rule, name) in candidate_rule_names(image_path) {
            let Some(paths) = names.get(&name.to_ascii_lowercase()) else {
                continue;
            };
            if paths.is_empty() {
                continue;
            }
            *rule_hits.entry(rule.clone()).or_default() += 1;
            let examples_for_rule = examples.entry(rule).or_default();
            if examples_for_rule.len() < 10 {
                examples_for_rule.push(CandidateRuleExample {
                    image_path: image_path.clone(),
                    candidate: paths[0].clone(),
                });
            }
            break;
        }
    }
    Ok(CandidateRulesSummary {
        unresolved_images: unresolved.len(),
        rule_hits,
        examples,
    })
}

fn wanted_names(path: &Path) -> anyhow::Result<BTreeMap<String, BTreeSet<String>>> {
    let text = fs::read_to_string(path)?;
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty TSV: {}", path.display()))?;
    let image_path_idx = header
        .split('\t')
        .position(|field| field == "image_path")
        .ok_or_else(|| anyhow::anyhow!("missing image_path column: {}", path.display()))?;
    let mut out = BTreeMap::<String, BTreeSet<String>>::new();
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        let Some(image_path) = fields.get(image_path_idx).map(|v| v.trim()) else {
            continue;
        };
        if image_path.is_empty() {
            continue;
        }
        let tail = image_path
            .split_once(':')
            .map(|(_, tail)| tail)
            .unwrap_or(image_path)
            .replace('\\', "/");
        let Some(name) = Path::new(&tail).file_name().and_then(|v| v.to_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let name_path = Path::new(name);
        let stem = name_path.file_stem().and_then(|v| v.to_str()).unwrap_or("");
        let suffix = name_path.extension().and_then(|v| v.to_str()).unwrap_or("");
        for candidate in [
            name.to_string(),
            thumbnail_name(stem, suffix).unwrap_or_default(),
        ] {
            if !candidate.is_empty() {
                out.entry(candidate.to_ascii_lowercase())
                    .or_default()
                    .insert(image_path.to_string());
            }
        }
    }
    Ok(out)
}

fn wanted_c2c_md5_stems(path: &Path) -> anyhow::Result<BTreeSet<String>> {
    let text = fs::read_to_string(path)?;
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty TSV: {}", path.display()))?;
    let image_path_idx = header
        .split('\t')
        .position(|field| field == "image_path")
        .ok_or_else(|| anyhow::anyhow!("missing image_path column: {}", path.display()))?;
    let mut out = BTreeSet::new();
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        let Some(image_path) = fields.get(image_path_idx).map(|v| v.trim()) else {
            continue;
        };
        if !image_path.starts_with("UserDataImage:C2C") {
            continue;
        }
        let tail = image_path
            .split_once(':')
            .map(|(_, tail)| tail)
            .unwrap_or(image_path)
            .replace('\\', "/");
        let Some(stem) = Path::new(&tail).file_stem().and_then(|v| v.to_str()) else {
            continue;
        };
        let stem = stem.to_ascii_lowercase();
        if stem.len() == 32 && stem.chars().all(|ch| ch.is_ascii_hexdigit()) {
            out.insert(stem);
        }
    }
    Ok(out)
}

fn unresolved_image_paths_from_audit(path: &Path) -> anyhow::Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty TSV: {}", path.display()))?;
    let headers = header.split('\t').collect::<Vec<_>>();
    let csv_idx = header_index(&headers, "csv", path)?;
    let rowid_idx = header_index(&headers, "rowid", path)?;
    let exists_idx = header_index(&headers, "exists", path)?;
    let image_path_idx = header_index(&headers, "image_path", path)?;
    let mut grouped = BTreeMap::<(String, String), Vec<Vec<String>>>::new();
    for line in lines {
        let fields = line.split('\t').map(str::to_string).collect::<Vec<_>>();
        let csv = fields.get(csv_idx).cloned().unwrap_or_default();
        let rowid = fields.get(rowid_idx).cloned().unwrap_or_default();
        grouped.entry((csv, rowid)).or_default().push(fields);
    }
    let mut out = Vec::new();
    for rows in grouped.values() {
        if rows
            .iter()
            .any(|row| row.get(exists_idx).map(|v| v == "1").unwrap_or(false))
        {
            continue;
        }
        if let Some(image_path) = rows
            .iter()
            .filter_map(|row| row.get(image_path_idx))
            .find(|value| !value.is_empty())
        {
            out.push(image_path.clone());
        }
    }
    Ok(out)
}

fn header_index(headers: &[&str], name: &str, path: &Path) -> anyhow::Result<usize> {
    headers
        .iter()
        .position(|field| *field == name)
        .ok_or_else(|| anyhow::anyhow!("missing {name} column: {}", path.display()))
}

fn candidate_rule_names(image_path: &str) -> Vec<(String, String)> {
    let tail = image_path
        .split_once(':')
        .map(|(_, tail)| tail)
        .unwrap_or(image_path)
        .replace('\\', "/");
    let Some(basename) = Path::new(&tail).file_name().and_then(|v| v.to_str()) else {
        return Vec::new();
    };
    let base = Path::new(basename);
    let stem = base.file_stem().and_then(|v| v.to_str()).unwrap_or("");
    let suffix = base
        .extension()
        .and_then(|v| v.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default();
    vec![
        ("basename_exact".to_string(), basename.to_string()),
        ("basename_tmb".to_string(), format!("{stem}_tmb{suffix}")),
        (
            "basename_lower_exact".to_string(),
            basename.to_ascii_lowercase(),
        ),
        (
            "basename_lower_tmb".to_string(),
            format!(
                "{}_tmb{}",
                stem.to_ascii_lowercase(),
                suffix.to_ascii_lowercase()
            ),
        ),
    ]
}

fn thumbnail_name(stem: &str, suffix: &str) -> Option<String> {
    if stem.is_empty() {
        return None;
    }
    if suffix.is_empty() {
        Some(format!("{stem}_tmb"))
    } else {
        Some(format!("{stem}_tmb.{suffix}"))
    }
}

fn scan_root_for_names(
    root: &Path,
    hits: &mut BTreeMap<String, BTreeSet<String>>,
) -> anyhow::Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let Some(name) = path.file_name().and_then(|v| v.to_str()) else {
                    continue;
                };
                let key = name.to_ascii_lowercase();
                if let Some(paths) = hits.get_mut(&key) {
                    paths.insert(path.display().to_string());
                }
            }
        }
    }
    Ok(())
}

fn scan_root_for_md5(
    root: &Path,
    wanted: &BTreeSet<String>,
    hashed: &mut usize,
    hits: &mut Vec<(String, String)>,
) -> anyhow::Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let Ok(digest) = md5_file(&path) else {
                    continue;
                };
                *hashed += 1;
                if wanted.contains(&digest) {
                    hits.push((digest, path.display().to_string()));
                }
            }
        }
    }
    Ok(())
}

fn scan_immediate_file_names(
    root: &Path,
    names: &mut BTreeMap<String, Vec<String>>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|v| v.to_str()) else {
            continue;
        };
        names
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(path.display().to_string());
    }
    Ok(())
}

fn md5_file(path: &Path) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut ctx = md5::Context::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.consume(&buf[..n]);
    }
    Ok(format!("{:x}", ctx.compute()))
}

fn write_tsv(path: &Path, header: &[&str], rows: &[Vec<String>]) -> anyhow::Result<()> {
    let mut out = String::new();
    out.push_str(&header.join("\t"));
    out.push('\n');
    for row in rows {
        out.push_str(
            &row.iter()
                .map(|field| field.replace(['\t', '\r', '\n'], " "))
                .collect::<Vec<_>>()
                .join("\t"),
        );
        out.push('\n');
    }
    fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_name_preserves_extension() {
        assert_eq!(thumbnail_name("abc", "png").unwrap(), "abc_tmb.png");
        assert_eq!(thumbnail_name("abc", "").unwrap(), "abc_tmb");
    }

    #[test]
    fn filters_c2c_md5_stems() {
        let dir = std::env::temp_dir().join(format!("qq_asset_audit_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unresolved.tsv");
        fs::write(
            &path,
            "image_path\nUserDataImage:C2C\\Image2\\0123456789ABCDEF0123456789ABCDEF.png\nUserDataImage:Group\\Image2\\ffffffffffffffffffffffffffffffff.png\n",
        )
        .unwrap();
        let stems = wanted_c2c_md5_stems(&path).unwrap();
        assert_eq!(stems.len(), 1);
        assert!(stems.contains("0123456789abcdef0123456789abcdef"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidate_rule_names_include_tmb_variants() {
        let names = candidate_rule_names("UserDataImage:C2C\\Image2\\ABC.png");
        assert_eq!(
            names[0],
            ("basename_exact".to_string(), "ABC.png".to_string())
        );
        assert_eq!(
            names[1],
            ("basename_tmb".to_string(), "ABC_tmb.png".to_string())
        );
        assert_eq!(
            names[3],
            ("basename_lower_tmb".to_string(), "abc_tmb.png".to_string())
        );
    }
}

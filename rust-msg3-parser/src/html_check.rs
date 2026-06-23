use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

const LINK_ATTRS: [&str; 3] = ["href", "src", "poster"];
const SKIP_SCHEMES: [&str; 7] = [
    "http",
    "https",
    "ftp",
    "mailto",
    "tel",
    "data",
    "javascript",
];

#[derive(Clone, Debug, Serialize)]
pub struct LinkCheckReport {
    pub root: String,
    pub html_files: usize,
    pub links_total: usize,
    pub local_links: usize,
    pub alive_local: usize,
    pub dead_local: usize,
    pub external_or_data: usize,
    pub anchors_only: usize,
    pub dead: Vec<DeadLink>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeadLink {
    pub html: String,
    pub tag: String,
    pub attr: String,
    pub link: String,
    pub resolved: String,
}

pub fn check_links(root: &Path) -> anyhow::Result<LinkCheckReport> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let html_files = collect_html_files(&root)?;
    let mut report = LinkCheckReport {
        root: root.display().to_string(),
        html_files: html_files.len(),
        links_total: 0,
        local_links: 0,
        alive_local: 0,
        dead_local: 0,
        external_or_data: 0,
        anchors_only: 0,
        dead: Vec::new(),
    };

    for html_file in html_files {
        let text = fs::read_to_string(&html_file).unwrap_or_else(|_| {
            String::from_utf8_lossy(&fs::read(&html_file).unwrap_or_default()).to_string()
        });
        for link in extract_links(&text) {
            report.links_total += 1;
            if link.value.starts_with('#') {
                report.anchors_only += 1;
                continue;
            }
            let Some(target) = resolve_local(html_file.parent().unwrap_or(&root), &link.value)
            else {
                report.external_or_data += 1;
                continue;
            };
            report.local_links += 1;
            if target.exists() {
                report.alive_local += 1;
            } else {
                report.dead_local += 1;
                report.dead.push(DeadLink {
                    html: html_file.display().to_string(),
                    tag: link.tag,
                    attr: link.attr,
                    link: link.value,
                    resolved: target.display().to_string(),
                });
            }
        }
    }
    Ok(report)
}

pub fn default_report_path(root: &Path) -> PathBuf {
    root.join("dead_links_report.json")
}

fn collect_html_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_html_files_inner(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_html_files_inner(path: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_file() {
        if is_html(path) {
            out.push(path.to_path_buf());
        }
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        if entry.file_type()?.is_dir() {
            collect_html_files_inner(&child, out)?;
        } else if is_html(&child) {
            out.push(child);
        }
    }
    Ok(())
}

fn is_html(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("html"))
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
struct Link {
    tag: String,
    attr: String,
    value: String,
}

fn extract_links(text: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let bytes = text.as_bytes();
    let mut pos = 0usize;
    while let Some(start_rel) = text[pos..].find('<') {
        let start = pos + start_rel;
        if start + 1 >= bytes.len() {
            break;
        }
        let next = bytes[start + 1];
        if matches!(next, b'!' | b'/' | b'?') {
            pos = start + 1;
            continue;
        }
        let Some(end_rel) = text[start..].find('>') else {
            break;
        };
        let end = start + end_rel;
        let tag_text = &text[start + 1..end];
        links.extend(extract_links_from_tag(tag_text));
        pos = end + 1;
    }
    links
}

fn extract_links_from_tag(tag_text: &str) -> Vec<Link> {
    let mut chars = tag_text.char_indices().peekable();
    while let Some((_, ch)) = chars.peek().copied() {
        if ch.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    let tag_start = chars.peek().map(|(idx, _)| *idx).unwrap_or(0);
    while let Some((_, ch)) = chars.peek().copied() {
        if ch.is_whitespace() || ch == '/' {
            break;
        }
        chars.next();
    }
    let tag_end = chars.peek().map(|(idx, _)| *idx).unwrap_or(tag_text.len());
    let tag = tag_text[tag_start..tag_end].to_ascii_lowercase();
    let mut links = Vec::new();
    let mut pos = tag_end;
    while pos < tag_text.len() {
        skip_ws(tag_text, &mut pos);
        if pos >= tag_text.len() || tag_text.as_bytes()[pos] == b'/' {
            break;
        }
        let name_start = pos;
        while pos < tag_text.len() {
            let b = tag_text.as_bytes()[pos];
            if b.is_ascii_whitespace() || b == b'=' || b == b'/' {
                break;
            }
            pos += 1;
        }
        let name = tag_text[name_start..pos].to_ascii_lowercase();
        skip_ws(tag_text, &mut pos);
        if pos >= tag_text.len() || tag_text.as_bytes()[pos] != b'=' {
            continue;
        }
        pos += 1;
        skip_ws(tag_text, &mut pos);
        let value = read_attr_value(tag_text, &mut pos);
        if LINK_ATTRS.contains(&name.as_str()) && !value.is_empty() {
            links.push(Link {
                tag: tag.clone(),
                attr: name,
                value,
            });
        }
    }
    links
}

fn skip_ws(text: &str, pos: &mut usize) {
    while *pos < text.len() && text.as_bytes()[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
}

fn read_attr_value(text: &str, pos: &mut usize) -> String {
    if *pos >= text.len() {
        return String::new();
    }
    let quote = text.as_bytes()[*pos];
    if quote == b'\'' || quote == b'"' {
        *pos += 1;
        let start = *pos;
        while *pos < text.len() && text.as_bytes()[*pos] != quote {
            *pos += 1;
        }
        let value = html_unescape(&text[start..(*pos).min(text.len())]);
        if *pos < text.len() {
            *pos += 1;
        }
        value
    } else {
        let start = *pos;
        while *pos < text.len() {
            let b = text.as_bytes()[*pos];
            if b.is_ascii_whitespace() || b == b'/' {
                break;
            }
            *pos += 1;
        }
        html_unescape(&text[start..*pos])
    }
}

fn resolve_local(base: &Path, link: &str) -> Option<PathBuf> {
    if link.starts_with("//") {
        return None;
    }
    let without_fragment = link.split('#').next().unwrap_or("");
    let path_part = without_fragment.split('?').next().unwrap_or("");
    if path_part.is_empty() {
        return None;
    }
    if let Some(colon) = path_part.find(':') {
        let scheme = &path_part[..colon].to_ascii_lowercase();
        if SKIP_SCHEMES.contains(&scheme.as_str()) {
            return None;
        }
    }
    let decoded = percent_decode(path_part);
    let path = PathBuf::from(decoded);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(
            base.join(path)
                .canonicalize()
                .unwrap_or_else(|_| base.join(percent_decode(path_part))),
        )
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut pos = 0usize;
    while pos < bytes.len() {
        if bytes[pos] == b'%' && pos + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[pos + 1]), hex_val(bytes[pos + 2])) {
                out.push((hi << 4) | lo);
                pos += 3;
                continue;
            }
        }
        out.push(bytes[pos]);
        pos += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_local_links_from_common_attrs() {
        let links =
            extract_links(r#"<a href="a.html">x</a><img SRC='img/a.png'><video poster=p.jpg>"#);
        let values = links.into_iter().map(|link| link.value).collect::<Vec<_>>();
        assert_eq!(values, vec!["a.html", "img/a.png", "p.jpg"]);
    }

    #[test]
    fn skips_external_and_decodes_local_paths() {
        assert!(resolve_local(Path::new("/tmp"), "https://example.com/a").is_none());
        assert_eq!(
            resolve_local(Path::new("/tmp/base"), "a%20b.html#top").unwrap(),
            PathBuf::from("/tmp/base/a b.html")
        );
    }
}

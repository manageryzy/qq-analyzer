use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use tiny_http::{Header, Response};

pub(crate) fn asset_href(path: &Path) -> String {
    let encoded =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path.to_string_lossy().as_bytes());
    format!("/asset/{encoded}")
}

pub(crate) fn serve_asset_response(
    root: &Path,
    path: &str,
) -> anyhow::Result<Response<std::io::Cursor<Vec<u8>>>> {
    let encoded = path
        .trim_start_matches("/asset/")
        .split('/')
        .next()
        .unwrap_or("");
    let padding = "=".repeat((4 - encoded.len() % 4) % 4);
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded.as_bytes())
        .or_else(|_| {
            base64::engine::general_purpose::URL_SAFE
                .decode(format!("{encoded}{padding}").as_bytes())
        })?;
    let asset_path = PathBuf::from(String::from_utf8_lossy(&decoded).to_string());
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let asset_path = asset_path.canonicalize()?;
    if !asset_path.starts_with(&root) {
        anyhow::bail!("asset outside root");
    }
    let data = fs::read(&asset_path)?;
    let mime = content_type_for_path(&asset_path, &data);
    Ok(Response::from_data(data).with_header(Header::from_bytes("Content-Type", mime).unwrap()))
}

fn content_type_for_path(path: &Path, data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return "image/gif";
    }
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        return "image/webp";
    }
    if data.starts_with(b"BM") {
        return "image/bmp";
    }
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "amr" => "audio/amr",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn href_roundtrip_uses_asset_prefix() {
        let href = asset_href(Path::new("dir/file name.png"));
        assert!(href.starts_with("/asset/"));
        assert!(!href.contains('='));
    }

    #[test]
    fn content_type_prefers_magic_bytes() {
        assert_eq!(
            content_type_for_path(Path::new("x.bin"), b"\x89PNG\r\n\x1a\nrest"),
            "image/png"
        );
        assert_eq!(
            content_type_for_path(Path::new("x.dat"), &[0xff, 0xd8, 0xff, 0x00]),
            "image/jpeg"
        );
        assert_eq!(content_type_for_path(Path::new("x.mp4"), b""), "video/mp4");
    }
}

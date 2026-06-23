use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

const CFB_MAGIC: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";
const FREESECT: u32 = 0xffff_ffff;
const ENDOFCHAIN: u32 = 0xffff_fffe;
const NO_STREAM: u32 = 0xffff_ffff;

#[derive(Clone, Debug)]
struct DirEntry {
    name: String,
    typ: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExtractedStream {
    pub stream: String,
    pub output: String,
    pub size: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CfbExtractReport {
    pub input: String,
    pub output_root: String,
    pub streams: usize,
    pub bytes: usize,
    pub files: Vec<ExtractedStream>,
}

pub fn is_cfb(path: &Path) -> bool {
    fs::read(path)
        .map(|data| data.starts_with(CFB_MAGIC))
        .unwrap_or(false)
}

pub fn extract_cfb(path: &Path, out_root: &Path, clean: bool) -> anyhow::Result<CfbExtractReport> {
    extract_cfb_with_limit(path, out_root, clean, None)
}

pub fn extract_cfb_with_limit(
    path: &Path,
    out_root: &Path,
    clean: bool,
    stream_limit: Option<usize>,
) -> anyhow::Result<CfbExtractReport> {
    if clean && out_root.exists() {
        fs::remove_dir_all(out_root)?;
    }
    fs::create_dir_all(out_root)?;
    let reader = CfbReader::open(path)?;
    let streams = reader.list_streams();
    let prefixes = stream_prefixes(&streams);
    let mut files = Vec::new();
    let mut total = 0usize;
    for (stream, entry_index) in streams.into_iter().take(stream_limit.unwrap_or(usize::MAX)) {
        let data = reader.read_stream_entry(entry_index)?;
        let out = unique_output_path(out_root, &stream, &prefixes);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&out, &data)?;
        total += data.len();
        files.push(ExtractedStream {
            stream,
            output: out.display().to_string(),
            size: data.len(),
        });
    }
    Ok(CfbExtractReport {
        input: path.display().to_string(),
        output_root: out_root.display().to_string(),
        streams: files.len(),
        bytes: total,
        files,
    })
}

struct CfbReader {
    data: Vec<u8>,
    sector_size: usize,
    mini_sector_size: usize,
    num_fat_sectors: u32,
    first_directory_sector: u32,
    mini_stream_cutoff: u32,
    first_mini_fat_sector: u32,
    num_mini_fat_sectors: u32,
    first_difat_sector: u32,
    num_difat_sectors: u32,
    max_sector: u32,
    fat: Vec<u32>,
    entries: Vec<DirEntry>,
    mini_fat: Vec<u32>,
    root_mini_stream: Vec<u8>,
}

impl CfbReader {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let data = fs::read(path)?;
        if !data.starts_with(CFB_MAGIC) {
            anyhow::bail!("not a CFB file: {}", path.display());
        }
        if data.len() < 512 {
            anyhow::bail!("CFB header too short: {}", path.display());
        }
        let sector_size = 1usize << read_u16(&data, 30)? as usize;
        let mini_sector_size = 1usize << read_u16(&data, 32)? as usize;
        if sector_size != 512 {
            anyhow::bail!("unsupported CFB sector size: {sector_size}");
        }
        let num_fat_sectors = read_u32(&data, 44)?;
        let first_directory_sector = read_u32(&data, 48)?;
        let mini_stream_cutoff = read_u32(&data, 56)?;
        let first_mini_fat_sector = read_u32(&data, 60)?;
        let num_mini_fat_sectors = read_u32(&data, 64)?;
        let first_difat_sector = read_u32(&data, 68)?;
        let num_difat_sectors = read_u32(&data, 72)?;
        let max_sector = data.len().saturating_div(sector_size).saturating_sub(1) as u32;

        let mut reader = Self {
            data,
            sector_size,
            mini_sector_size,
            num_fat_sectors,
            first_directory_sector,
            mini_stream_cutoff,
            first_mini_fat_sector,
            num_mini_fat_sectors,
            first_difat_sector,
            num_difat_sectors,
            max_sector,
            fat: Vec::new(),
            entries: Vec::new(),
            mini_fat: Vec::new(),
            root_mini_stream: Vec::new(),
        };
        let fat_sector_ids = reader.read_difat()?;
        reader.fat = reader.read_fat(&fat_sector_ids);
        reader.entries = reader.read_directory()?;
        reader.mini_fat = reader.read_mini_fat()?;
        reader.root_mini_stream = reader.read_root_mini_stream();
        Ok(reader)
    }

    fn valid_sector(&self, sector: u32) -> bool {
        sector <= self.max_sector
    }

    fn sector_bytes(&self, sector: u32) -> &[u8] {
        if !self.valid_sector(sector) {
            return &[];
        }
        let off = (sector as usize + 1) * self.sector_size;
        let end = off.saturating_add(self.sector_size).min(self.data.len());
        self.data.get(off..end).unwrap_or(&[])
    }

    fn read_difat(&self) -> anyhow::Result<Vec<u32>> {
        let mut ids = Vec::new();
        let mut seen = BTreeSet::new();
        let add = |ids: &mut Vec<u32>, seen: &mut BTreeSet<u32>, value: u32| {
            if matches!(value, FREESECT | ENDOFCHAIN) {
                return;
            }
            if self.valid_sector(value) && seen.insert(value) {
                ids.push(value);
            }
        };
        for i in 0..109 {
            add(&mut ids, &mut seen, read_u32(&self.data, 76 + i * 4)?);
        }
        let mut sector = self.first_difat_sector;
        let mut visited = BTreeSet::new();
        for _ in 0..self.num_difat_sectors {
            if matches!(sector, FREESECT | ENDOFCHAIN)
                || !self.valid_sector(sector)
                || !visited.insert(sector)
            {
                break;
            }
            let raw = self.sector_bytes(sector);
            if raw.len() != self.sector_size {
                break;
            }
            for i in 0..127 {
                add(&mut ids, &mut seen, read_u32(raw, i * 4)?);
            }
            sector = read_u32(raw, 127 * 4)?;
        }
        if self.num_fat_sectors > 0 {
            ids.truncate(self.num_fat_sectors as usize);
        }
        Ok(ids)
    }

    fn read_fat(&self, fat_sector_ids: &[u32]) -> Vec<u32> {
        let mut fat = Vec::new();
        for sector in fat_sector_ids {
            let raw = self.sector_bytes(*sector);
            if raw.len() == self.sector_size {
                for chunk in raw.chunks_exact(4) {
                    fat.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
            }
        }
        fat
    }

    fn chain(&self, start: u32, limit: Option<usize>) -> Vec<u32> {
        if matches!(start, FREESECT | ENDOFCHAIN) || !self.valid_sector(start) {
            return Vec::new();
        }
        let max_steps = limit.unwrap_or(self.max_sector as usize + 1);
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut sector = start;
        while out.len() < max_steps {
            if matches!(sector, FREESECT | ENDOFCHAIN)
                || !self.valid_sector(sector)
                || !seen.insert(sector)
            {
                break;
            }
            out.push(sector);
            let Some(next) = self.fat.get(sector as usize).copied() else {
                break;
            };
            sector = next;
        }
        out
    }

    fn read_sector_chain(&self, start: u32, size: Option<usize>, limit: Option<usize>) -> Vec<u8> {
        let mut out = Vec::new();
        for sector in self.chain(start, limit) {
            out.extend_from_slice(self.sector_bytes(sector));
        }
        if let Some(size) = size {
            out.truncate(size);
        }
        out
    }

    fn read_directory(&self) -> anyhow::Result<Vec<DirEntry>> {
        let raw = self.read_sector_chain(self.first_directory_sector, None, None);
        let mut entries = Vec::new();
        for (idx, item) in raw.chunks_exact(128).enumerate() {
            let name_len = u16::from_le_bytes([item[64], item[65]]) as usize;
            let typ = item[66];
            if !matches!(typ, 1 | 2 | 5) || name_len < 2 || name_len > 64 || name_len % 2 != 0 {
                let _ = idx;
                entries.push(empty_dir_entry());
                continue;
            }
            let name = String::from_utf16_lossy(
                &item[..name_len - 2]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect::<Vec<_>>(),
            );
            let size_low = u32::from_le_bytes([item[120], item[121], item[122], item[123]]) as u64;
            let size_high = u32::from_le_bytes([item[124], item[125], item[126], item[127]]) as u64;
            entries.push(DirEntry {
                name,
                typ,
                left: u32::from_le_bytes([item[68], item[69], item[70], item[71]]),
                right: u32::from_le_bytes([item[72], item[73], item[74], item[75]]),
                child: u32::from_le_bytes([item[76], item[77], item[78], item[79]]),
                start: u32::from_le_bytes([item[116], item[117], item[118], item[119]]),
                size: size_low | (size_high << 32),
            });
        }
        Ok(entries)
    }

    fn read_mini_fat(&self) -> anyhow::Result<Vec<u32>> {
        if matches!(self.first_mini_fat_sector, FREESECT | ENDOFCHAIN)
            || self.num_mini_fat_sectors == 0
        {
            return Ok(Vec::new());
        }
        let raw = self.read_sector_chain(
            self.first_mini_fat_sector,
            None,
            Some(self.num_mini_fat_sectors as usize),
        );
        Ok(raw
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }

    fn read_root_mini_stream(&self) -> Vec<u8> {
        let Some(root) = self.entries.first() else {
            return Vec::new();
        };
        if matches!(root.start, FREESECT | ENDOFCHAIN) || root.size == 0 {
            return Vec::new();
        }
        self.read_sector_chain(root.start, Some(root.size as usize), None)
    }

    fn mini_chain(&self, start: u32) -> Vec<u32> {
        if matches!(start, FREESECT | ENDOFCHAIN) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut sector = start;
        while !matches!(sector, FREESECT | ENDOFCHAIN) {
            if sector as usize >= self.mini_fat.len() || !seen.insert(sector) {
                break;
            }
            out.push(sector);
            sector = self.mini_fat[sector as usize];
        }
        out
    }

    fn read_stream_entry(&self, index: usize) -> anyhow::Result<Vec<u8>> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("invalid CFB directory index {index}"))?;
        if entry.size == 0 || matches!(entry.start, FREESECT | ENDOFCHAIN) {
            return Ok(Vec::new());
        }
        if entry.size < self.mini_stream_cutoff as u64 && !self.root_mini_stream.is_empty() {
            let mut out = Vec::new();
            for mini_sector in self.mini_chain(entry.start) {
                let off = mini_sector as usize * self.mini_sector_size;
                let end = off
                    .saturating_add(self.mini_sector_size)
                    .min(self.root_mini_stream.len());
                if off < end {
                    out.extend_from_slice(&self.root_mini_stream[off..end]);
                }
            }
            out.truncate(entry.size as usize);
            return Ok(out);
        }
        Ok(self.read_sector_chain(entry.start, Some(entry.size as usize), None))
    }

    fn list_streams(&self) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        let mut visited = BTreeSet::new();
        if !self.entries.is_empty() {
            self.walk(0, &[], &mut visited, &mut out);
        }
        if out.is_empty() {
            out = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.name.is_empty() && e.typ == 2)
                .map(|(i, e)| (e.name.clone(), i))
                .collect();
        }
        out
    }

    fn walk(
        &self,
        idx: u32,
        prefix: &[String],
        visited: &mut BTreeSet<u32>,
        out: &mut Vec<(String, usize)>,
    ) {
        if idx == NO_STREAM || idx as usize >= self.entries.len() || !visited.insert(idx) {
            return;
        }
        let entry = &self.entries[idx as usize];
        if entry.name.is_empty() || !matches!(entry.typ, 1 | 2 | 5) {
            return;
        }
        self.walk(entry.left, prefix, visited, out);
        match entry.typ {
            1 => {
                let mut next = prefix.to_vec();
                next.push(entry.name.clone());
                self.walk(entry.child, &next, visited, out);
            }
            2 => {
                let mut parts = prefix.to_vec();
                parts.push(entry.name.clone());
                out.push((parts.join("/"), idx as usize));
            }
            5 => self.walk(entry.child, prefix, visited, out),
            _ => {}
        }
        self.walk(entry.right, prefix, visited, out);
    }
}

fn empty_dir_entry() -> DirEntry {
    DirEntry {
        name: String::new(),
        typ: 0,
        left: NO_STREAM,
        right: NO_STREAM,
        child: NO_STREAM,
        start: FREESECT,
        size: 0,
    }
}

fn stream_prefixes(streams: &[(String, usize)]) -> BTreeSet<Vec<String>> {
    let mut out = BTreeSet::new();
    for (stream, _) in streams {
        let parts = stream
            .split('/')
            .filter(|part| !part.is_empty())
            .map(safe_part)
            .collect::<Vec<_>>();
        for i in 1..parts.len() {
            out.insert(parts[..i].to_vec());
        }
    }
    out
}

fn unique_output_path(out_root: &Path, stream: &str, prefixes: &BTreeSet<Vec<String>>) -> PathBuf {
    let mut parts = stream
        .split('/')
        .filter(|part| !part.is_empty())
        .map(safe_part)
        .collect::<Vec<_>>();
    if prefixes.contains(&parts) {
        parts.push("__stream".to_string());
    }
    let mut base = out_root.to_path_buf();
    for part in parts {
        base.push(part);
    }
    if !base.exists() {
        return base;
    }
    let parent = base.parent().unwrap_or(out_root);
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("_")
        .to_string();
    let suffix = base.extension().and_then(|s| s.to_str()).unwrap_or("");
    for i in 1..1_000_000usize {
        let name = if suffix.is_empty() {
            format!("{stem}__{i}")
        } else {
            format!("{stem}__{i}.{suffix}")
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem}__overflow"))
}

fn safe_part(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_control() || matches!(ch, '\\' | '/' | ':' | '<' | '>' | '|' | '?' | '*') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim().trim_end_matches(['.', ' ']).to_string();
    let mut safe = if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed
    };
    if safe.chars().count() > 180 {
        safe = safe.chars().take(180).collect();
    }
    safe
}

fn read_u16(data: &[u8], offset: usize) -> anyhow::Result<u16> {
    let bytes = data
        .get(offset..offset + 2)
        .ok_or_else(|| anyhow::anyhow!("CFB read_u16 out of bounds at {offset}"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> anyhow::Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow::anyhow!("CFB read_u32 out of bounds at {offset}"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

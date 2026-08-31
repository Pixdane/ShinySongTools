//! UnityFS 资源路线实验:离线扫描下载的资产包,定位歌词/文本类资产。
//!
//! 下载存储 `Documents/D/<2ch>/<hash>` 中的文件形态(2026-08-30 实测):
//! - 135,894 个 UnityFS(12.2GB,明文,头前有 4 字节长度前缀)
//! - 32,322 个 LZ4 frame 压缩的 JSON(MV 演出数据,无歌词)
//! - CRIWARE 视频(@UFF/CRID/AFS2)
//!
//! 本程序:解析 UnityFS(archive v8,Unity 6000.1.16f1)头 → blocksinfo →
//! 分块解压 → 在解压数据中搜索歌词特征(`lyric` 对象名、假名密集的 UTF-16
//! 段、UTF-8 日文行),报告命中的 bundle。只读下载文件,不碰游戏进程。
//!
//! v8 头布局(真机样本逐字节验证,详见 README):与 v6/v7 完全不同,
//! 没有 typetree 字段;`header_size` 从 BUNDLE 起点数且等于文件长度减前缀。

use lz4_flex::block::decompress_into as lz4_block_decompress_into;
use rayon::prelude::*;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const KANA: &[(u16, u16)] = &[(0x3041, 0x3096), (0x30A1, 0x30F6)];

fn be32(raw: &[u8], off: usize) -> Option<u32> {
    let b = raw.get(off..off + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn is_kana(c: char) -> bool {
    let u = c as u32;
    KANA.iter()
        .any(|(lo, hi)| u >= u32::from(*lo) && u <= u32::from(*hi))
}

fn is_cjk(c: char) -> bool {
    (0x4E00..=0x9FFF).contains(&(c as u32))
}

struct Bundle {
    raw: Vec<u8>,
    /// (data offset, compressed len, uncompressed len, flags),按块顺序。
    blocks: Vec<(usize, usize, usize, u16)>,
    info_pad: usize,
    data_pad: usize,
}

/// 解析 Unity 6000.1.16f1 archive-v8 bundle。
///
/// 已验证布局(全 BE,以 2907 字节样本 D/3K/3KB45VRU… 为准):
/// ```text
///   0: u32 LE wrapper 前缀(语义未定,忽略)
///   4: "UnityFS\0"
///  12: u32 version = 8
///  16: cstring unity("5.x.x\0") + cstring engine("6000.1.16f1\0")
///  34: u32 target platform
///  38: u32 header_size(从 BUNDLE 起点数,== 文件长度 - 4)
///  42: u32 compressed blocksinfo size
///  46: u32 uncompressed blocksinfo size
///  50: u32 flags(实测 0x243:压缩 = LZ4)
///  54+14 = 68: LZ4 raw-block 压缩的 blocksinfo(14 字节 pad,两样本一致)
/// ```
/// blocksinfo 明文:16 字节 hash 全零,`u32 block_count @16`,其后块表
/// `{u32 uncomp; u32 comp; u16 flags}` stride 10;数据块紧随压缩 blocksinfo
/// 之后(+pad,样本 1 实测 7)。info pad 与数据块 pad 均带候选回退:解压
/// 校验通过才接受。
fn parse_bundle(raw: Vec<u8>) -> Option<Bundle> {
    if raw.len() < 96 || raw[4..12] != *b"UnityFS\x00" {
        return None;
    }
    let mut pos = 16usize; // version 在 12..16
    for _ in 0..2 {
        pos += raw.get(pos..)?.iter().position(|&b| b == 0)? + 1;
    }
    let cinfo = be32(&raw, pos + 8)? as usize;
    let uinfo_size = be32(&raw, pos + 12)? as usize;
    let flags = be32(&raw, pos + 16)?;
    if flags & 0x80 != 0 || cinfo == 0 || uinfo_size < 24 || uinfo_size > 1 << 20 {
        return None;
    }
    let comp = (flags & 0x3F) as usize;
    if comp > 3 {
        return None;
    }
    for info_pad in [14usize, 2, 4, 6, 8, 10, 12, 16] {
        let info_start = pos + 20 + info_pad;
        let Some(info_raw) = raw.get(info_start..info_start + cinfo) else {
            continue;
        };
        let info = match comp {
            0 => info_raw.to_vec(),
            1 => {
                let mut out = Vec::new();
                lzma_rs::lzma_decompress(&mut std::io::Cursor::new(info_raw), &mut out).ok()?;
                out
            }
            _ => {
                let mut out = vec![0u8; uinfo_size];
                let n = lz4_block_decompress_into(info_raw, &mut out).ok()?;
                out.truncate(n);
                out
            }
        };
        let Some(bcount) = be32(&info, 16).map(|v| v as usize) else {
            continue;
        };
        if bcount == 0 || bcount > 65_536 || info.len() < 20 + bcount * 10 {
            continue;
        }
        let mut blocks = Vec::with_capacity(bcount);
        for i in 0..bcount {
            let p = 20 + i * 10;
            let uncomp = be32(&info, p)? as usize;
            let clen = be32(&info, p + 4)? as usize;
            let bflags = u16::from_be_bytes([info[p + 8], info[p + 9]]);
            blocks.push((0usize, clen, uncomp, bflags));
        }
        let info_end = info_start + cinfo;
        let total_comp: usize = blocks.iter().map(|b| b.1).sum();
        let first_comp = (blocks[0].3 & 0x3F) as usize;
        for data_pad in 0..=24usize {
            let coff = info_end + data_pad;
            if coff + total_comp > raw.len() {
                break;
            }
            if first_comp != 0 {
                // 用首块解压校验 pad:pad 不对几乎必然解压失败。
                let sample = &raw[coff..coff + blocks[0].1];
                let mut out = vec![0u8; blocks[0].2];
                let ok = match first_comp {
                    2 | 3 => lz4_block_decompress_into(sample, &mut out).is_ok(),
                    _ => false,
                };
                if !ok {
                    continue;
                }
            }
            let mut off = coff;
            for b in &mut blocks {
                b.0 = off;
                off += b.1;
            }
            return Some(Bundle {
                raw,
                blocks,
                info_pad,
                data_pad,
            });
        }
    }
    None
}

fn decompress_bundle(bundle: &Bundle) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    for (in_off, in_len, uncomp_len, bflags) in &bundle.blocks {
        let compressed = bundle.raw.get(*in_off..in_off + in_len)?;
        match (bflags & 0x3F) as usize {
            0 => data.extend_from_slice(compressed),
            2 | 3 => {
                let mut out = vec![0u8; *uncomp_len];
                lz4_block_decompress_into(compressed, &mut out).ok()?;
                data.extend_from_slice(&out);
            }
            1 => {
                let mut out = Vec::new();
                lzma_rs::lzma_decompress(&mut std::io::Cursor::new(compressed), &mut out).ok()?;
                data.extend_from_slice(&out);
            }
            _ => return None,
        }
    }
    Some(data)
}

/// `lyric`(ASCII,大小写不敏感)在解压数据中的位置,至多 cap 个。
fn find_lyric_positions(data: &[u8], cap: usize) -> Vec<usize> {
    let hay = data.to_ascii_lowercase();
    let mut positions = Vec::new();
    let mut start = 0usize;
    while let Some(rel) = hay[start..].windows(5).position(|w| w == b"lyric") {
        let abs = start + rel;
        positions.push(abs);
        start = abs + 5;
        if positions.len() >= cap {
            break;
        }
    }
    positions
}

/// UTF-16LE 连续假名段(≥ min_run 个 unit)的起始位置,至多 cap 个。
fn find_utf16_kana_positions(data: &[u8], min_run: usize, cap: usize) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let mut i = 0usize;
    while i + 1 < data.len() {
        let unit = u16::from_le_bytes([data[i], data[i + 1]]);
        let u = u32::from(unit);
        let kana = KANA
            .iter()
            .any(|(lo, hi)| u >= u32::from(*lo) && u <= u32::from(*hi));
        if kana {
            if run_len == 0 {
                run_start = i;
            }
            run_len += 1;
        } else {
            if run_len >= min_run && positions.len() < cap {
                positions.push(run_start);
            }
            run_len = 0;
        }
        i += 2;
    }
    if run_len >= min_run && positions.len() < cap {
        positions.push(run_start);
    }
    positions
}

/// 提取命中点附近的 UTF-8 日文行(TMP m_Text 之类序列化字符串)。
fn extract_utf8_lines(data: &[u8], around: usize, max: usize) -> Vec<String> {
    let from = around.saturating_sub(8192);
    let to = (around + 8192).min(data.len());
    let mut lines = Vec::new();
    for chunk in data[from..to].split(|&b| b == 0x0A || b == 0x00) {
        if chunk.len() < 8 || lines.len() >= max {
            continue;
        }
        let Ok(s) = std::str::from_utf8(chunk) else {
            continue;
        };
        let t = s.trim();
        if t.chars().count() >= 4 && t.chars().any(|c| is_kana(c) || is_cjk(c)) {
            lines.push(t.to_string());
        }
    }
    lines
}

/// 提取命中点附近的 UTF-16LE 日文段(前一会话确认的歌词文本形态)。
fn extract_utf16_lines(data: &[u8], around: usize, max: usize) -> Vec<String> {
    let from = around.saturating_sub(8192);
    let to = (around + 8192).min(data.len());
    let mut lines = Vec::new();
    let mut i = from;
    while i + 1 < to && lines.len() < max {
        let mut j = i;
        let mut units = 0usize;
        while j + 1 < to {
            let unit = u16::from_le_bytes([data[j], data[j + 1]]);
            let u = u32::from(unit);
            let ok = KANA
                .iter()
                .any(|(lo, hi)| u >= u32::from(*lo) && u <= u32::from(*hi))
                || (0x4E00..=0x9FFF).contains(&u)
                || unit == 0x0A
                || unit == 0x20;
            if ok {
                units += 1;
                j += 2;
            } else {
                break;
            }
        }
        if units >= 8 {
            let s: String = (i..j)
                .step_by(2)
                .filter_map(|k| char::from_u32(u16::from_le_bytes([data[k], data[k + 1]]) as u32))
                .collect();
            let t = s.trim().to_string();
            if t.chars().count() >= 4 {
                lines.push(t);
            }
            i = j + 2;
        } else {
            i += 2;
        }
    }
    lines
}

/// 命中点附近可打印 ASCII 字符串(英文歌词等)。
fn extract_ascii_strings(data: &[u8], around: usize, max: usize) -> Vec<String> {
    let from = around.saturating_sub(2048);
    let to = (around + 2048).min(data.len());
    let mut out = Vec::new();
    for chunk in data[from..to].split(|&b| !(0x20..0x7F).contains(&b)) {
        if chunk.len() >= 12 && out.len() < max {
            out.push(String::from_utf8_lossy(chunk).into_owned());
        }
    }
    out
}

/// 歌词行形态:短且假名/汉字混用。全假名或全汉字的长串是字体图集码表。
fn looks_like_lyric_line(s: &str) -> bool {
    s.chars().count() <= 48 && s.chars().any(is_kana) && s.chars().any(is_cjk)
}

/// 字体图集的假名码表:大量不同假名且码点几乎单调递增,不是歌词。
fn looks_like_glyph_table(s: &str) -> bool {
    let kana: Vec<u32> = s
        .chars()
        .filter(|c| is_kana(*c))
        .map(|c| c as u32)
        .collect();
    let distinct: std::collections::HashSet<_> = kana.iter().collect();
    if kana.len() < 24 || distinct.len() < 20 {
        return false;
    }
    let inc = kana.windows(2).filter(|w| w[1] > w[0]).count();
    inc * 10 >= kana.len() * 8
}

struct ScanHit {
    via_lyric_name: bool,
    lines: Vec<String>,
}

fn scan_bundle(raw: Vec<u8>, text_filter: Option<&str>) -> Option<ScanHit> {
    let bundle = parse_bundle(raw)?;
    let data = decompress_bundle(&bundle)?;
    if let Some(f) = text_filter {
        if !data.windows(f.len()).any(|w| w == f.as_bytes()) {
            return None;
        }
    }
    let name_positions = find_lyric_positions(&data, 8);
    let mut lines: Vec<String> = name_positions
        .iter()
        .flat_map(|p| extract_utf8_lines(&data, *p, 4).into_iter().take(2))
        .collect();
    lines.extend(
        name_positions
            .iter()
            .flat_map(|p| extract_utf16_lines(&data, *p, 2))
            .filter(|l| looks_like_lyric_line(l)),
    );
    if name_positions.is_empty() {
        // 无 lyric 对象名时退而求其次:假名密集的 UTF-16 段也算歌词特征。
        let kana_positions = find_utf16_kana_positions(&data, 12, 2);
        let kana_lines: Vec<String> = kana_positions
            .iter()
            .flat_map(|p| extract_utf16_lines(&data, *p, 3))
            .filter(|l| looks_like_lyric_line(l) && !looks_like_glyph_table(l))
            .collect();
        if !kana_lines.is_empty() {
            lines.extend(kana_lines);
        }
    }
    lines.truncate(8);
    if lines.is_empty() {
        return None;
    }
    Some(ScanHit {
        via_lyric_name: !name_positions.is_empty(),
        lines,
    })
}

/// 只完整读取 UnityFS 文件;其他形态(CRI 视频、LZ4 JSON)只看 12 字节魔数。
fn read_if_unityfs(path: &Path) -> Option<Vec<u8>> {
    let mut f = File::open(path).ok()?;
    let mut magic = [0u8; 12];
    f.read_exact(&mut magic).ok()?;
    if magic[4..12] != *b"UnityFS\x00" {
        return None;
    }
    let mut raw = magic.to_vec();
    f.read_to_end(&mut raw).ok()?;
    Some(raw)
}

fn collect_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .flat_map(|e| std::fs::read_dir(e.path()).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

fn inspect_single(path: &Path, needle: Option<&str>) {
    let Ok(raw) = std::fs::read(path) else {
        println!("read failed");
        return;
    };
    let Some(bundle) = parse_bundle(raw) else {
        println!("parse failed");
        return;
    };
    let uncomp: usize = bundle.blocks.iter().map(|b| b.2).sum();
    println!(
        "parsed: blocks={} info_pad={} data_pad={} total uncompressed={uncomp}",
        bundle.blocks.len(),
        bundle.info_pad,
        bundle.data_pad
    );
    let Some(data) = decompress_bundle(&bundle) else {
        println!("parse ok, decompress failed");
        return;
    };
    println!("decompressed bytes: {}", data.len());
    if let Some(needle) = needle {
        // 子串搜索:打印每次出现附近的 ASCII 字符串(调试用)。
        let mut start = 0usize;
        let mut hits = 0;
        while let Some(rel) = data[start..]
            .windows(needle.len())
            .position(|w| w == needle.as_bytes())
        {
            let abs = start + rel;
            println!("\"{needle}\"@{abs:#x}");
            for l in extract_ascii_strings(&data, abs, 8) {
                println!("   asc {l}");
            }
            for l in extract_utf8_lines(&data, abs, 4) {
                println!("   u8  {l}");
            }
            start = abs + needle.len();
            hits += 1;
            if hits >= 5 {
                break;
            }
        }
        if hits == 0 {
            println!("\"{needle}\" not found");
        }
        return;
    }
    let positions = find_lyric_positions(&data, 5);
    if positions.is_empty() {
        println!("no lyric-name hit in this bundle");
    }
    for p in &positions {
        println!("lyric@{p:#x}");
        for l in extract_ascii_strings(&data, *p, 6) {
            println!("   asc {l}");
        }
        for l in extract_utf8_lines(&data, *p, 4) {
            println!("   u8  {l}");
        }
        for l in extract_utf16_lines(&data, *p, 2) {
            println!("   u16 {}", l.replace('\n', " ⏎ "));
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(target) = args.get(1).cloned() else {
        eprintln!("usage: resources-route <D目录或单个bundle文件> [文本过滤]");
        std::process::exit(2);
    };
    let text_filter = args.get(2).cloned();
    let path = Path::new(&target);
    if path.is_file() {
        inspect_single(path, text_filter.as_deref());
        return;
    }
    let files = collect_files(path);
    println!("scanning {} files", files.len());

    if std::env::var("STATS").is_ok() {
        let stats: (u64, u64, u64, u64) = files
            .par_iter()
            .map(|p| match read_if_unityfs(p) {
                None => (1, 0, 0, 0),
                Some(raw) => match parse_bundle(raw).map(|b| decompress_bundle(&b)) {
                    Some(Some(_)) => (1, 1, 1, 1),
                    Some(None) => (1, 1, 1, 0),
                    None => (1, 1, 0, 0),
                },
            })
            .reduce(
                || (0, 0, 0, 0),
                |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2, a.3 + b.3),
            );
        println!(
            "files-scanned: {}, unityfs: {}, header-parsed: {}, fully-decompressed: {}",
            stats.0, stats.1, stats.2, stats.3
        );
        return;
    }

    let results: Vec<(PathBuf, ScanHit)> = files
        .par_iter()
        .filter_map(|p| {
            let raw = read_if_unityfs(p)?;
            let hit = scan_bundle(raw, text_filter.as_deref())?;
            Some((p.clone(), hit))
        })
        .collect();
    let via_name = results.iter().filter(|(_, h)| h.via_lyric_name).count();
    println!(
        "bundles with lyric features: {} (lyric-name: {}, utf16-kana-only: {})",
        results.len(),
        via_name,
        results.len() - via_name
    );
    for (p, h) in &results {
        let tag = if h.via_lyric_name {
            ""
        } else {
            "  [utf16-kana-only]"
        };
        println!("=== {}{tag}", p.display());
        for l in &h.lines {
            println!("   {}", l.replace('\n', " ⏎ "));
        }
    }
}

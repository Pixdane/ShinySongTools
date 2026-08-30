//! UnityFS 资源路线实验:离线扫描下载的资产包,定位歌词/文本类资产。
//!
//! 下载存储 `Documents/D/<2ch>/<hash>` 中的文件形态(2026-08-30 实测):
//! - 135,894 个 UnityFS(12.2GB,明文,头前有 4 字节长度前缀)
//! - 32,322 个 LZ4 frame 压缩的 JSON(MV 演出数据,无歌词)
//! - CRIWARE 视频(@UFF/CRID/AFS2)
//!
//! 本程序:解析 UnityFS 头 → blocksinfo → 分块解压 → 在解压数据中搜索
//! `lyric` 对象名与假名密集段(歌词行特征),报告命中的 bundle。
//! 只读下载文件,不碰游戏进程。

use lz4_flex::block::decompress_into as lz4_block_decompress_into;
use rayon::prelude::*;
use std::io::Read;
use std::path::PathBuf;

const KANA: &[(u16, u16)] = &[(0x3041, 0x3096), (0x30A1, 0x30F6)];

fn has_kana_run(bytes: &[u8], min_run: usize) -> bool {
    let mut run = 0usize;
    let bytes_len = bytes.len();
    let mut i = 0usize;
    while i + 1 < bytes_len {
        let unit = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        if KANA.iter().any(|(lo, hi)| unit >= *lo && unit <= *hi) {
            run += 1;
            if run >= min_run {
                return true;
            }
        } else {
            run = 0;
        }
        i += 2;
    }
    false
}

struct Bundle {
    raw: Vec<u8>,
    blocks: Vec<(usize, usize, usize, u16)>, // (in_off, in_len, uncomp_len, flags)
    blocksinfo_off: usize,
    blocksinfo_len: usize,
}

fn parse_bundle(raw: Vec<u8>) -> Option<Bundle> {
    if raw.len() < 64 || raw[4..12] != *b"UnityFS\x00" {
        return None;
    }
    let mut pos = 12usize; // 4-byte prefix + "UnityFS\0"
    let _version = i32::from_be_bytes(raw[pos..pos + 4].try_into().ok()?);
    pos += 4;
    for _ in 0..2 {
        while raw[pos] != 0 {
            pos += 1;
        }
        pos += 1; // cstring + NUL
    }
    let _target = i32::from_be_bytes(raw[pos..pos + 4].try_into().ok()?);
    pos += 4;
    pos += 1; // enable type tree
    let header_size = u32::from_be_bytes(raw[pos..pos + 4].try_into().ok()?) as usize;
    let compressed_info_size = u32::from_be_bytes(raw[pos + 4..pos + 8].try_into().ok()?) as usize;
    let uncompressed_info_size =
        u32::from_be_bytes(raw[pos + 8..pos + 12].try_into().ok()?) as usize;
    let flags = u32::from_be_bytes(raw[pos + 12..pos + 16].try_into().ok()?);
    pos += 16;

    let info_at_end = flags & 0x40 != 0;
    let compression = (flags & 0x3F) as usize;
    let info_start = if info_at_end {
        raw.len() - compressed_info_size
    } else {
        pos + header_size
    };
    let info_raw = raw.get(info_start..info_start + compressed_info_size)?;
    let info = match compression {
        0 => info_raw.to_vec(),
        1 => {
            let mut out = Vec::with_capacity(uncompressed_info_size);
            lzma_rs::lzma_decompress(&mut std::io::Cursor::new(info_raw), &mut out).ok()?;
            out
        }
        2 | 3 => {
            let mut out = vec![0u8; uncompressed_info_size];
            lz4_block_decompress_into(info_raw, &mut out).ok()?;
            out
        }
        _ => return None,
    };
    if info.len() < 20 {
        return None;
    }
    let bcount = u32::from_be_bytes(info[16..20].try_into().ok()?) as usize;
    let mut blocks = Vec::with_capacity(bcount);
    let mut ipos = 20usize;
    let mut coff = if info_at_end {
        pos + header_size
    } else {
        info_start + compressed_info_size
    };
    for _ in 0..bcount {
        if ipos + 10 > info.len() {
            return None;
        }
        let u_size = u32::from_be_bytes(info[ipos..ipos + 4].try_into().ok()?) as usize;
        let c_size = u32::from_be_bytes(info[ipos + 4..ipos + 8].try_into().ok()?) as usize;
        let bflags = u16::from_be_bytes(info[ipos + 8..ipos + 10].try_into().ok()?);
        blocks.push((coff, c_size, u_size, bflags));
        coff += c_size;
        ipos += 10;
    }
    Some(Bundle {
        raw,
        blocks,
        blocksinfo_off: info_start,
        blocksinfo_len: compressed_info_size,
    })
}

fn decompress_bundle(bundle: &Bundle) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    for (in_off, in_len, uncomp_len, bflags) in &bundle.blocks {
        let compressed = bundle.raw.get(*in_off..in_off + in_len)?;
        let compression = (bflags & 0x3F) as usize;
        match compression {
            0 => data.extend_from_slice(compressed),
            2 | 3 => {
                let mut out = vec![0u8; *uncomp_len];
                lz4_block_decompress_into(compressed, &mut out).ok()?;
                data.extend_from_slice(&out);
            }
            1 => {
                let mut out = Vec::with_capacity(*uncomp_len);
                lzma_rs::lzma_decompress(&mut std::io::Cursor::new(compressed), &mut out).ok()?;
                data.extend_from_slice(&out);
            }
            _ => return None,
        }
    }
    Some(data)
}

/// 提取 `lyric` 命中附近的候选歌词行(UTF-16LE 日文段)。
fn extract_candidate_lines(data: &[u8], around: usize) -> Vec<String> {
    let from = around.saturating_sub(8192);
    let to = (around + 8192).min(data.len());
    let mut lines = Vec::new();
    let mut i = from;
    while i + 1 < to {
        // UTF-16LE 字符串探测:连续假名/汉字/空白 ≥ 8 个单元
        let mut j = i;
        let mut units = 0usize;
        while j + 1 < to {
            let unit = u16::from_le_bytes([data[j], data[j + 1]]);
            let ok = KANA.iter().any(|(lo, hi)| unit >= *lo && unit <= *hi)
                || (0x4E00..=0x9FFF).contains(&unit)
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
            if s.trim().len() >= 4 && lines.len() < 6 {
                lines.push(s.trim().to_string());
            }
            i = j + 2;
        } else {
            i += 2;
        }
    }
    lines
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| {
        "/Users/pixdane/Library/Containers/jp.co.bandainamcoent.BNEI0416/Data/Documents/D".into()
    });
    let sid_filter = args.get(2).cloned();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(|e| e.ok())
        .flat_map(|e| std::fs::read_dir(e.path()).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    files.sort();
    println!("scanning {} files", files.len());

    let results: Vec<(PathBuf, Vec<String>)> = files
        .par_iter()
        .filter_map(|path| {
            let raw = std::fs::read(path).ok()?;
            let bundle = parse_bundle(raw)?;
            let data = decompress_bundle(&bundle)?;
            let hay = data.to_ascii_lowercase();
            let mut positions = Vec::new();
            let mut start = 0usize;
            while let Some(rel) = hay[start..].windows(5).position(|w| w == b"lyric") {
                let abs = start + rel;
                positions.push(abs);
                start = abs + 5;
                if positions.len() >= 8 {
                    break;
                }
            }
            if positions.is_empty() {
                return None;
            }
            if let Some(sid) = &sid_filter {
                // 只保留包含指定 sid 附近文本的命中
                if !hay.windows(sid.len()).any(|w| w == sid.as_bytes()) {
                    return None;
                }
            }
            let lines = positions
                .iter()
                .flat_map(|p| extract_candidate_lines(&data, *p))
                .take(8)
                .collect::<Vec<_>>();
            if lines.is_empty() {
                None
            } else {
                Some((path.clone(), lines))
            }
        })
        .collect();

    println!("bundles with lyric-name + kana text: {}", results.len());
    for (path, lines) in results.iter().take(30) {
        println!("=== {}", path.display());
        for l in lines {
            println!("   {}", l.replace('\n', " ⏎ "));
        }
    }
}

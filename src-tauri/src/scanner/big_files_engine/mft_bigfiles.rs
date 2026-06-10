// ============================================================================
// MFT 大文件扫描器 — USN 枚举 + 路径重建 + 用户目录过滤 stat + Top-N
// ============================================================================

#![cfg(target_os = "windows")]

use std::cmp::Reverse;
use std::collections::BinaryHeap;
// use std::io::Write;
use std::time::Instant;

use log::info;
use rayon::prelude::*;

use crate::scanner::big_files::{compute_file_risk_level, compute_source_label, LargeFileEntry};
use crate::scanner::big_files_engine::mft_core;

const SKIP_EXTS: &[&str] = &[
    "dll", "sys", "ini", "inf", "cat", "manifest", "lnk", "url", "ico", "cur", "ani",
    "mui", "mun", "etl", "log", "tmp", "bak", "xml", "json", "cfg", "reg", "bat", "cmd",
    "ps1", "vbs", "js", "css", "htm", "html", "txt", "md", "nls", "ttf", "otf", "fon",
];

const SKIP_PATH_SEGMENTS: &[&str] = &[
    "\\Windows\\System32\\",
    "\\Windows\\SysWOW64\\",
    "\\Windows\\WinSxS\\",
    "\\Windows\\assembly\\",
    "\\Windows\\Microsoft.NET\\",
];

/// 只 stat 这些路径段下的文件（用户大文件高发区）
const SCAN_PATH_KEYWORDS: &[&str] = &[
    "\\Users\\", "\\Downloads\\", "\\Desktop\\", "\\Documents\\",
    "\\Videos\\", "\\Pictures\\", "\\Music\\", "\\Games\\",
    "\\SteamLibrary\\", "\\Steam\\", "\\Epic Games\\",
    "\\Program Files\\", "\\Program Files (x86)\\",
    "\\AppData\\Local\\", "\\AppData\\Roaming\\",
    "\\OneDrive\\", "\\Dropbox\\", "\\Google Drive\\",
    "\\迅雷下载\\",
];

fn is_small_ext(name: &str) -> bool {
    name.rsplit('.').next().map_or(false, |e| SKIP_EXTS.iter().any(|&x| x.eq_ignore_ascii_case(e)))
}

fn is_system_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    SKIP_PATH_SEGMENTS.iter().any(|s| p.contains(s))
}

fn is_user_path(path: &str) -> bool {
    SCAN_PATH_KEYWORDS.iter().any(|&kw| path.contains(kw))
}

pub fn scan_top_files_via_mft(
    top_n: usize,
    progress_cb: impl Fn(usize),
) -> Result<Vec<LargeFileEntry>, String> {
    // DEBUG: 需要文件日志时取消下面注释
    // let mut log_file = std::fs::OpenOptions::new().create(true).append(true).open("C:\\mft_debug.log").ok();
    macro_rules! flog {
        ($($arg:tt)*) => {
            info!($($arg)*);
            // if let Some(ref mut f) = log_file { let _ = writeln!(f, "[{}] {}", std::time::UNIX_EPOCH.elapsed().unwrap_or_default().as_secs(), format!($($arg)*)); let _ = f.flush(); }
        };
    }

    let t0 = Instant::now();
    let system_drive = std::env::var("SYSTEMDRIVE").unwrap_or_else(|_| "C:".to_string());
    let drive_letter = system_drive.chars().next().unwrap_or('C');

    flog!("[MFT-BigFiles] ===== 扫描开始 top_n={} =====", top_n);

    // Step 1: USN 枚举
    flog!("[MFT-BigFiles] Step1 USN 枚举...");
    let h_device = mft_core::open_volume(drive_letter)?;
    let entries = mft_core::enumerate_usn_records_v2(h_device, &progress_cb)?;
    mft_core::close_volume(h_device);
    let t1 = Instant::now();
    flog!("[MFT-BigFiles] Step1: {} 条, {:.1}s", entries.len(), t1.duration_since(t0).as_secs_f32());

    if entries.is_empty() { return Err("USN 空".into()); }

    // Step 2: 重建全路径
    flog!("[MFT-BigFiles] Step2 路径重建...");
    let paths = mft_core::rebuild_full_paths(&entries, drive_letter);
    let t2 = Instant::now();
    flog!("[MFT-BigFiles] Step2: {} 条, {:.1}s", paths.len(), t2.duration_since(t1).as_secs_f32());

    // Step 3: 预过滤 + 用户目录白名单 + 系统目录黑名单 + 并行 stat
    let file_total = entries.iter().filter(|e| !e.is_dir).count();
    let candidates: Vec<_> = entries.iter()
        .filter(|e| !e.is_dir && !is_small_ext(&e.name))
        .collect();
    flog!("[MFT-BigFiles] Step3 文件{} → 候选{}", file_total, candidates.len());

    let sized: Vec<(u64, i64, String)> = candidates
        .par_iter()
        .filter_map(|e| {
            let path = paths.get(&e.mft_id)?;
            if is_system_path(path) || !is_user_path(path) { return None; }
            let m = std::fs::metadata(path).ok()?;
            let s = m.len();
            if s == 0 { return None; }
            let modified = m.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64).unwrap_or(0);
            Some((s, modified, path.clone()))
        })
        .collect();

    let t3 = Instant::now();
    flog!("[MFT-BigFiles] Step3 stat: {} 个, {:.1}s (t={})", sized.len(), t3.duration_since(t2).as_secs_f32(), rayon::current_num_threads());

    // Step 4: BinaryHeap Top-N
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
    for (idx, (size, _, _)) in sized.iter().enumerate() {
        heap.push(Reverse((*size, idx)));
        if heap.len() > top_n { heap.pop(); }
    }

    // Step 5: risk/source
    progress_cb(0);
    let mut results: Vec<LargeFileEntry> = heap.into_iter().map(|Reverse((_, idx))| {
        let (size, modified, path) = &sized[idx];
        LargeFileEntry { path: path.clone(), size: *size, modified: *modified,
            risk_level: compute_file_risk_level(path), source_label: compute_source_label(path) }
    }).collect();
    results.sort_by(|a, b| b.size.cmp(&a.size));

    let t4 = Instant::now();
    flog!("[MFT-BigFiles] ===== 完成: Top-{}, 总 {:.1}s =====", results.len(), t4.duration_since(t0).as_secs_f32());
    flog!("[MFT-BigFiles] 枚举={:.1}s, 路径={:.1}s, stat={:.1}s, TopN={:.1}s",
        t1.duration_since(t0).as_secs_f32(), t2.duration_since(t1).as_secs_f32(),
        t3.duration_since(t2).as_secs_f32(), t4.duration_since(t3).as_secs_f32());

    Ok(results)
}

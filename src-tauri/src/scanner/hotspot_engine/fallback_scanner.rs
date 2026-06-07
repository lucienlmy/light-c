// ============================================================================
// 降级扫描引擎 — jwalk 文件系统遍历 + 祖先聚合
// 当无法使用 MFT 直读（非管理员 / 非 NTFS）时的回退方案
// 从 hotspot.rs 的 aggregate_ancestor_stats 迁移而来
// ============================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use jwalk::WalkDir as JWalkDir;

use crate::scanner::hotspot::{is_heavy_system_dir, is_hidden_by_path, FolderStats, HotspotScanner};

/// jwalk 并行遍历 + 祖先聚合（核心扫描引擎）
///
/// 使用 jwalk 替代 walkdir 的关键优势：
/// - 多线程并行列出目录内容，抵消 Win11 Defender 单次 IO 延迟
/// - DirEntry 内部缓存 metadata，后续 `.metadata()` 零开销
/// - process_read_dir 回调在列目录阶段预过滤，防止进入 WinSxS 等巨型目录
///
/// 每个文件向上聚合到**所有**祖先目录，确保每层目录的 total_size 包含全部后代。
///
/// 返回 (根目录统计, 所有祖先目录统计 map)
pub fn aggregate_ancestor_stats(
    root: &Path,
    max_depth: u8,
    track_modified: bool,
    cancel_flag: &AtomicBool,
    ignore_system_dirs: bool,
) -> (FolderStats, HashMap<PathBuf, FolderStats>) {
    let mut root_stats = FolderStats {
        total_size: 0,
        file_count: 0,
        last_modified: 0,
    };
    let mut ancestor_map: HashMap<PathBuf, FolderStats> = HashMap::new();

    // jwalk 并行目录遍历，process_read_dir 在列目录后、递归前过滤
    let walker = JWalkDir::new(root)
        .max_depth(max_depth as usize)
        .skip_hidden(false)
        .process_read_dir(move |_depth, _path, _state, children| {
            // 预过滤：阻止 jwalk 进入巨型系统目录和隐藏目录
            // 当用户关闭系统目录过滤时，WinSxS/DriverStore 等也允许进入扫描
            children.retain(|dir_entry_result| {
                dir_entry_result.as_ref().map(|e| {
                    let p = e.path();
                    let skip_heavy = ignore_system_dirs && is_heavy_system_dir(&p);
                    !skip_heavy && !is_hidden_by_path(&p)
                }).unwrap_or(false) // 读取失败的条目无法进入，安全移除
            });
        })
        .into_iter();

    for entry in walker {
        // 取消检查
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }

        let e = match entry {
            Ok(e) => e,
            Err(_) => continue, // 权限拒绝等错误静默跳过
        };

        // 只处理文件
        if !e.file_type().is_file() {
            continue;
        }

        // jwalk 的 metadata() 是缓存的，不会触发额外 syscall
        let metadata = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let file_size = metadata.len();

        // 仅在 Accurate 模式下收集修改时间
        let modified_ts = if track_modified {
            metadata
                .modified()
                .ok()
                .map(|t| HotspotScanner::system_time_to_millis(t))
                .unwrap_or(0)
        } else {
            0
        };

        // 累加到根目录
        root_stats.total_size += file_size;
        root_stats.file_count += 1;
        if track_modified && modified_ts > root_stats.last_modified {
            root_stats.last_modified = modified_ts;
        }

        // 向上聚合到所有祖先目录（确保每层目录都包含所有后代文件的大小）
        if let Ok(relative) = e.path().strip_prefix(root) {
            let comp_count = relative.components().count();
            let dir_ancestors = comp_count.saturating_sub(1);

            let mut current = root.to_path_buf();
            for comp in relative.components().take(dir_ancestors) {
                current.push(comp);
                let ancestor = ancestor_map
                    .entry(current.clone())
                    .or_insert_with(|| FolderStats {
                        total_size: 0,
                        file_count: 0,
                        last_modified: 0,
                    });
                ancestor.total_size += file_size;
                ancestor.file_count += 1;
                if track_modified && modified_ts > ancestor.last_modified {
                    ancestor.last_modified = modified_ts;
                }
            }
        }
    }

    (root_stats, ancestor_map)
}

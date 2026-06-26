use crate::ai_models::model_file_rules::{
    is_model_package_directory, is_supported_model_extension,
};
use crate::ai_models::types::{AssetSource, ModelItem};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

pub fn user_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub fn normalize_existing_path(path: PathBuf) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }

    // canonicalize 可以合并同一路径的不同写法，失败时保留原路径避免权限问题中断扫描。
    Some(path.canonicalize().unwrap_or(path))
}

pub fn unique_existing_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut unique_paths = Vec::new();

    for path in paths {
        if let Some(existing_path) = normalize_existing_path(path) {
            let key = existing_path.to_string_lossy().to_lowercase();
            if seen.insert(key) {
                unique_paths.push(existing_path);
            }
        }
    }

    unique_paths
}

pub fn directory_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

pub fn file_size(path: &Path) -> Option<u64> {
    path.metadata()
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
}

pub fn is_model_extension(path: &Path) -> bool {
    is_supported_model_extension(path)
}

pub fn collect_model_files(root: &Path) -> Vec<ModelItem> {
    let mut models: Vec<ModelItem> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(skip_hidden_system_noise)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if entry.file_type().is_dir() && is_model_package_directory(path) {
                // Core ML 的 .mlpackage 是目录包，不会被文件扩展名逻辑命中，需要按目录整体计入资产。
                let size = directory_size(path);
                if size > 0 {
                    return Some(ModelItem {
                        name: path
                            .file_name()
                            .and_then(|value| value.to_str())
                            .unwrap_or("未命名模型包")
                            .to_string(),
                        size,
                        path: path.to_path_buf(),
                    });
                }
            }

            if !entry.file_type().is_file() {
                return None;
            }

            if !is_model_extension(path) {
                return None;
            }

            let size = file_size(path)?;

            Some(ModelItem {
                name: path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("未命名模型")
                    .to_string(),
                size,
                path: path.to_path_buf(),
            })
        })
        .collect();

    models.sort_by(|left, right| right.size.cmp(&left.size));
    models
}

pub fn source_from_models(
    name: &str,
    path: PathBuf,
    mut models: Vec<ModelItem>,
) -> Option<AssetSource> {
    if models.is_empty() {
        return None;
    }

    models.sort_by(|left, right| right.size.cmp(&left.size));
    let total_size = models.iter().map(|model| model.size).sum();

    Some(AssetSource {
        name: name.to_string(),
        path,
        total_size,
        model_count: models.len(),
        models,
    })
}

fn skip_hidden_system_noise(entry: &DirEntry) -> bool {
    let file_name = entry.file_name().to_string_lossy();

    // 这些目录不会承载用户可管理的模型资产，跳过可以减少递归成本和误判。
    !matches!(
        file_name.as_ref(),
        ".git" | "node_modules" | "target" | "$RECYCLE.BIN" | "System Volume Information"
    )
}

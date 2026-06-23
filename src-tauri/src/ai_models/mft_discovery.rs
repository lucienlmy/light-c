#![cfg(target_os = "windows")]

use crate::ai_models::types::{AssetSource, ModelItem};
use crate::scanner::big_files_engine::mft_core;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const SAFETENSORS_MIN_SIZE: u64 = 100 * 1024 * 1024;
const GGUF_MIN_SIZE: u64 = 100 * 1024 * 1024;
const CKPT_MIN_SIZE: u64 = 500 * 1024 * 1024;
const NOISY_MODEL_MIN_SIZE: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CoveredRoot {
    pub source_name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
struct MftModelCandidate {
    mft_id: u64,
    size: u64,
}

pub fn discover_models_via_mft(covered_roots: &[CoveredRoot]) -> (Vec<AssetSource>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut grouped_models: HashMap<String, Vec<ModelItem>> = HashMap::new();

    for drive_letter in local_drive_letters() {
        if !mft_core::is_ntfs(drive_letter) {
            continue;
        }

        match scan_drive_for_model_candidates(drive_letter, covered_roots) {
            Ok(models) => {
                for (source_name, model) in models {
                    grouped_models.entry(source_name).or_default().push(model);
                }
            }
            Err(error) => warnings.push(format!("{}: {}", drive_letter, error)),
        }
    }

    let mut sources = grouped_models
        .into_iter()
        .filter_map(|(source_name, mut models)| {
            if models.is_empty() {
                return None;
            }

            models.sort_by(|left, right| right.size.cmp(&left.size));
            let total_size = models.iter().map(|model| model.size).sum();
            let source_path = common_parent_path(&models).unwrap_or_default();

            Some(AssetSource {
                name: source_name,
                path: source_path,
                total_size,
                model_count: models.len(),
                models,
            })
        })
        .collect::<Vec<_>>();

    sources.sort_by(|left, right| right.total_size.cmp(&left.total_size));
    (sources, warnings)
}

fn scan_drive_for_model_candidates(
    drive_letter: char,
    covered_roots: &[CoveredRoot],
) -> Result<Vec<(String, ModelItem)>, String> {
    let handle = mft_core::open_volume(drive_letter)?;
    let entries_result = mft_core::enumerate_usn_records_v2(handle, &|_| true);
    mft_core::close_volume(handle);
    let entries = entries_result?;

    let candidate_ids = entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .filter_map(|entry| {
            // MFT 兜底只关注少量大模型扩展名，先在 USN 文件名层面过滤，避免对全盘文件都读取大小。
            model_extension_threshold(&entry.name).map(|min_size| (entry.mft_id, min_size))
        })
        .collect::<HashMap<_, _>>();

    if candidate_ids.is_empty() {
        return Ok(Vec::new());
    }

    let reader = mft_core::NtfsFileMetadataReader::open(drive_letter)?;
    let wanted_ids = candidate_ids.keys().copied().collect::<HashSet<_>>();
    let metadata_map = reader.read_file_metadata_map(&wanted_ids, &|_| true)?;

    let retained_ids = metadata_map
        .iter()
        .filter_map(|(mft_id, metadata)| {
            let min_size = *candidate_ids.get(mft_id)?;
            (metadata.size >= min_size).then_some(*mft_id)
        })
        .collect::<HashSet<_>>();

    if retained_ids.is_empty() {
        return Ok(Vec::new());
    }

    let paths = mft_core::rebuild_paths_for_ids(&entries, drive_letter, &retained_ids);
    let mut models = Vec::new();

    for MftModelCandidate { mft_id, size } in retained_ids.iter().filter_map(|mft_id| {
        metadata_map.get(mft_id).map(|metadata| MftModelCandidate {
            mft_id: *mft_id,
            size: metadata.size,
        })
    }) {
        let Some(path) = paths.get(&mft_id).map(PathBuf::from) else {
            continue;
        };
        if should_skip_path(&path) {
            continue;
        }

        if is_covered_by_config_layer(&path, covered_roots) {
            continue;
        }

        let source_name = infer_source_name(&path, covered_roots);
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("未知模型")
            .to_string();

        models.push((source_name, ModelItem { name, size, path }));
    }

    Ok(models)
}

fn is_covered_by_config_layer(path: &Path, covered_roots: &[CoveredRoot]) -> bool {
    // 配置层是高置信来源，MFT 只负责补漏；这里直接跳过已覆盖路径，避免平台列表和兜底结果重复出现。
    covered_roots
        .iter()
        .any(|root| path_starts_with(path, &root.path))
}

fn model_extension_threshold(file_name: &str) -> Option<u64> {
    let extension = Path::new(file_name)
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();

    match extension.as_str() {
        "safetensors" => Some(SAFETENSORS_MIN_SIZE),
        "gguf" => Some(GGUF_MIN_SIZE),
        "ckpt" => Some(CKPT_MIN_SIZE),
        // .bin/.pt/.pth 误判率高，MFT 兜底里必须提高阈值，只做大体积模型候选。
        "bin" | "pt" | "pth" => Some(NOISY_MODEL_MIN_SIZE),
        _ => None,
    }
}

fn infer_source_name(path: &Path, covered_roots: &[CoveredRoot]) -> String {
    if let Some(root) = covered_roots
        .iter()
        .find(|root| path_starts_with(path, &root.path))
    {
        return root.source_name.clone();
    }

    let lower_path = path.to_string_lossy().to_ascii_lowercase();
    if lower_path.contains("\\diffusion_models\\")
        || lower_path.contains("\\checkpoints\\")
        || lower_path.contains("\\loras\\")
        || lower_path.contains("\\text_encoders\\")
        || lower_path.contains("\\controlnet\\")
        || lower_path.contains("\\vae\\")
    {
        return "ComfyUI".to_string();
    }

    if lower_path.contains("\\hub\\models--") || lower_path.contains("\\huggingface\\") {
        return "HuggingFace".to_string();
    }

    if lower_path.contains("\\.lmstudio\\") {
        return "LM Studio".to_string();
    }

    if lower_path.contains("\\.ollama\\models\\") {
        return "Ollama".to_string();
    }

    "未知来源".to_string()
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    let path_key = normalize_path_key(path);
    let mut root_key = normalize_path_key(root);
    if !root_key.ends_with('\\') {
        root_key.push('\\');
    }

    path_key == root_key.trim_end_matches('\\') || path_key.starts_with(&root_key)
}

fn normalize_path_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn should_skip_path(path: &Path) -> bool {
    let lower_path = path.to_string_lossy().to_ascii_lowercase();
    lower_path.contains("\\windows\\")
        || lower_path.contains("\\program files\\")
        || lower_path.contains("\\program files (x86)\\")
        || lower_path.contains("\\node_modules\\")
        || lower_path.contains("\\target\\")
        || lower_path.contains("\\.git\\")
        || lower_path.contains("\\$recycle.bin\\")
        || lower_path.contains("\\system volume information\\")
}

fn common_parent_path(models: &[ModelItem]) -> Option<PathBuf> {
    models.first().and_then(|model| {
        if model.path.is_file() {
            model.path.parent().map(Path::to_path_buf)
        } else {
            Some(model.path.clone())
        }
    })
}

fn local_drive_letters() -> Vec<char> {
    ('A'..='Z')
        .filter(|drive_letter| PathBuf::from(format!("{}:\\", drive_letter)).is_dir())
        .collect()
}

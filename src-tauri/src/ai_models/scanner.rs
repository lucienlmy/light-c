use crate::ai_models::detectors::create_detectors;
use crate::ai_models::mft_discovery::{discover_models_via_mft, CoveredRoot};
use crate::ai_models::types::{AiModelScanResult, AssetSource};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn scan_ai_model_assets(
    custom_paths: Vec<PathBuf>,
    enable_deep_discovery: bool,
) -> AiModelScanResult {
    let started_at = Instant::now();
    let detectors = create_detectors(custom_paths);

    let outputs: Vec<_> = detectors
        .into_par_iter()
        .map(|detector| detector.detect())
        .collect();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();

    for mut output in outputs {
        if let Some(source) = output.source.take() {
            sources.push(source);
        }
        warnings.append(&mut output.warnings);
    }

    let mut sources = dedupe_models_by_path(sources);
    if enable_deep_discovery {
        let covered_roots = covered_roots_from_sources(&sources);
        let (mut mft_sources, mut mft_warnings) = discover_models_via_mft(&covered_roots);
        sources.append(&mut mft_sources);
        warnings.append(&mut mft_warnings);
    }

    let sources = dedupe_models_by_path(sources);
    let sources = merge_sources_by_name(sources);
    let total_size = sources.iter().map(|source| source.total_size).sum();
    let total_model_count = sources.iter().map(|source| source.model_count).sum();

    AiModelScanResult {
        total_size,
        total_model_count,
        source_count: sources.len(),
        sources,
        warnings,
        scan_duration_ms: started_at.elapsed().as_millis(),
        discovery_mode: if enable_deep_discovery {
            "deep".to_string()
        } else {
            "quick".to_string()
        },
    }
}

fn covered_roots_from_sources(sources: &[AssetSource]) -> Vec<CoveredRoot> {
    let mut covered_roots = Vec::new();
    let mut seen_paths = HashSet::new();

    for source in sources
        .iter()
        .filter(|source| source.name != "自定义目录" && source.name != "未知来源")
    {
        // 平台 Detector 可能来自多个根目录，MFT 兜底需要把已识别模型的父目录也纳入覆盖范围，避免同一路径二次计数。
        push_covered_root(
            &mut covered_roots,
            &mut seen_paths,
            &source.name,
            source.path.clone(),
        );

        for model in &source.models {
            let covered_path = model
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| model.path.clone());
            push_covered_root(
                &mut covered_roots,
                &mut seen_paths,
                &source.name,
                covered_path,
            );
        }
    }

    covered_roots
}

fn push_covered_root(
    covered_roots: &mut Vec<CoveredRoot>,
    seen_paths: &mut HashSet<String>,
    source_name: &str,
    path: PathBuf,
) {
    if path.as_os_str().is_empty() {
        return;
    }

    let key = canonical_path_key(&path);
    if seen_paths.insert(key) {
        covered_roots.push(CoveredRoot {
            source_name: source_name.to_string(),
            path,
        });
    }
}

fn dedupe_models_by_path(mut sources: Vec<AssetSource>) -> Vec<AssetSource> {
    sources.sort_by_key(|source| source_priority(&source.name));
    let mut seen_model_keys = HashSet::new();
    let mut deduped_sources = Vec::new();

    for mut source in sources {
        source.models.retain(|model| {
            let key = model_identity_key(&source.name, &model.name, &model.path);
            seen_model_keys.insert(key)
        });

        if source.models.is_empty() {
            continue;
        }

        source
            .models
            .sort_by(|left, right| right.size.cmp(&left.size));
        source.model_count = source.models.len();
        source.total_size = source.models.iter().map(|model| model.size).sum();
        deduped_sources.push(source);
    }

    deduped_sources.sort_by(|left, right| right.total_size.cmp(&left.total_size));
    deduped_sources
}

fn merge_sources_by_name(sources: Vec<AssetSource>) -> Vec<AssetSource> {
    let mut merged_sources: Vec<AssetSource> = Vec::new();
    let mut seen_model_keys = HashSet::new();

    for source in sources {
        if let Some(existing_source) = merged_sources
            .iter_mut()
            .find(|item| item.name == source.name)
        {
            for model in source.models {
                let key = model_identity_key(&source.name, &model.name, &model.path);
                if seen_model_keys.insert(key) {
                    existing_source.models.push(model);
                }
            }
            existing_source
                .models
                .sort_by(|left, right| right.size.cmp(&left.size));
            existing_source.model_count = existing_source.models.len();
            existing_source.total_size =
                existing_source.models.iter().map(|model| model.size).sum();
        } else {
            for model in &source.models {
                let key = model_identity_key(&source.name, &model.name, &model.path);
                seen_model_keys.insert(key);
            }
            merged_sources.push(source);
        }
    }

    merged_sources.sort_by(|left, right| right.total_size.cmp(&left.total_size));
    merged_sources
}

fn source_priority(name: &str) -> u8 {
    match name {
        "Ollama" => 0,
        "ComfyUI" => 1,
        "HuggingFace" => 2,
        "LM Studio" => 3,
        _ => 4,
    }
}

fn model_identity_key(source_name: &str, model_name: &str, path: &Path) -> String {
    let path_key = canonical_path_key(path);

    // MFT 返回的路径在权限或文件移动场景下可能无法立即 metadata 成功，模型扩展名本身足以作为文件级去重依据。
    if path.is_file() || looks_like_model_file_path(path) {
        return path_key;
    }

    // Ollama 这类模型会指向共享目录，必须带上来源和模型名，避免多个 manifest 被误合并。
    format!("{}::{}::{}", source_name, model_name, path_key)
}

fn looks_like_model_file_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "gguf" | "safetensors" | "ckpt" | "onnx" | "pt" | "pth" | "bin"
            )
        })
        .unwrap_or(false)
}

fn canonical_path_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_lowercase()
}

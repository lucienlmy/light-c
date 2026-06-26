use super::{
    directory_size, source_from_models, unique_existing_paths, user_home_dir, DetectorOutput,
    ModelDetector,
};
use crate::ai_models::types::ModelItem;
use std::path::{Path, PathBuf};

pub struct HuggingFaceDetector;

impl HuggingFaceDetector {
    pub fn new() -> Self {
        Self
    }
}

impl ModelDetector for HuggingFaceDetector {
    fn detect(&self) -> DetectorOutput {
        let mut candidate_roots = Vec::new();

        if let Ok(hf_home) = std::env::var("HF_HOME") {
            // HuggingFace 官方允许通过 HF_HOME 改缓存根目录，优先读取它能覆盖大多数迁移到其他盘的用户。
            candidate_roots.push(PathBuf::from(hf_home));
        }

        if let Some(home_dir) = user_home_dir() {
            candidate_roots.push(home_dir.join(".cache").join("huggingface"));
        }

        let mut models = Vec::new();
        let mut source_path = None;
        for root in unique_existing_paths(candidate_roots) {
            source_path.get_or_insert_with(|| root.clone());
            models.extend(collect_huggingface_models(&root));
        }

        DetectorOutput {
            source: source_from_models("HuggingFace", source_path.unwrap_or_default(), models),
            warnings: Vec::new(),
        }
    }
}

fn collect_huggingface_models(root: &Path) -> Vec<ModelItem> {
    let hub_dir = root.join("hub");
    if !hub_dir.is_dir() {
        return Vec::new();
    }

    let Ok(entries) = hub_dir.read_dir() else {
        return Vec::new();
    };

    let mut models: Vec<ModelItem> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .map(|name| name.starts_with("models--"))
                .unwrap_or(false)
        })
        .filter_map(|path| {
            let size = directory_size(&path);
            if size == 0 {
                return None;
            }

            Some(ModelItem {
                name: huggingface_model_name(&path),
                size,
                path,
            })
        })
        .collect();

    models.sort_by(|left, right| right.size.cmp(&left.size));
    models
}

fn huggingface_model_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|name| {
            name.strip_prefix("models--")
                .unwrap_or(name)
                .replace("--", "/")
        })
        .unwrap_or_else(|| "HuggingFace 模型缓存".to_string())
}

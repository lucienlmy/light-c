use crate::ai_models::{
    scan_ai_model_assets_with_progress as scan_ai_model_assets_impl, AiModelScanResult,
};
use tauri::{AppHandle, Emitter};

#[tauri::command]
pub async fn scan_ai_model_assets(
    app_handle: AppHandle,
    enable_deep_discovery: Option<bool>,
) -> Result<AiModelScanResult, String> {
    let deep_discovery = enable_deep_discovery.unwrap_or(false);

    tokio::task::spawn_blocking(move || {
        scan_ai_model_assets_impl(deep_discovery, &|progress| {
            // AI 模型深度发现可能触发 MFT 兜底，阶段事件能让前端在长 IO 期间保持可解释反馈。
            let _ = app_handle.emit("ai-models:progress", &progress);
        })
    })
    .await
    .map_err(|error| format!("AI 资产扫描任务异常：{}", error))
}

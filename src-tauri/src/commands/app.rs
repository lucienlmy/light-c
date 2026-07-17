// ============================================================================
// 应用运行环境命令
// ============================================================================

pub use crate::runtime::DistributionChannel;

/// 获取当前发行渠道。
///
/// 发行模式由统一运行时解析器判断，保证更新、完整性校验和数据目录使用同一结果。
#[tauri::command]
pub fn get_distribution_channel() -> DistributionChannel {
    std::env::current_exe()
        .map(|exe_path| crate::runtime::detect_distribution_channel(&exe_path))
        .unwrap_or(DistributionChannel::Installer)
}

// ============================================================================
// 全盘增长分析模块
//
// 将 C 盘扫描、快照和差异分析集中在独立模块中，避免继续把全盘逻辑塞进
// ProgramData 专用规则里，后续维护扫描策略和对比策略会更清晰。
// ============================================================================

pub mod growth;
pub mod mft_scan;
pub mod snapshot;

pub use growth::*;
pub use mft_scan::{cancel_disk_growth_scan, reset_disk_growth_cancelled};

// ============================================================================
// 文件完整性校验命令
// ============================================================================

use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use serde::Serialize;
use std::path::{Path, PathBuf};

const PORTABLE_MARKER_FILE: &str = "LightC.portable";
const INSTALLER_EXE_SIGNATURE_ASSET: &str = "LightC_installer_exe.sig";
const PORTABLE_EXE_SIGNATURE_ASSET: &str = "LightC_portable_exe.sig";
const OFFICIAL_RELEASE_URL: &str = "https://github.com/Chunyu33/light-c/releases";
const UPDATER_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDU3NEJFNkU1NzM3OEQyQTQKUldTazBuaHo1ZVpMVnpKbnUrSnUrWlpVakhKL1c5ZXV3ZXhYeW4wbFRSeVFyb01TZ0h2RGpsZFoK";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyIntegrityStatus {
    Verified,
    Failed,
    NetworkError,
    ReleaseUnavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyIntegrityResult {
    pub verified: bool,
    pub status: VerifyIntegrityStatus,
    pub version: String,
    pub channel: String,
    pub message: String,
    pub official_url: String,
}

struct SignatureSource {
    signature: String,
}

/// 校验当前正在运行的 LightC.exe 是否由官方签名。
#[tauri::command]
pub async fn verify_integrity() -> VerifyIntegrityResult {
    match verify_integrity_inner().await {
        Ok(result) => result,
        Err(VerifyError::Network(message)) => build_result(
            false,
            VerifyIntegrityStatus::NetworkError,
            current_version(),
            current_channel_label(),
            format!("无法连接到 GitHub：{}", message),
        ),
        Err(VerifyError::ReleaseUnavailable(message)) => build_result(
            false,
            VerifyIntegrityStatus::ReleaseUnavailable,
            current_version(),
            current_channel_label(),
            message,
        ),
        Err(VerifyError::InvalidSignature(message)) => build_result(
            false,
            VerifyIntegrityStatus::Failed,
            current_version(),
            current_channel_label(),
            message,
        ),
        Err(VerifyError::Local(message)) => build_result(
            false,
            VerifyIntegrityStatus::Failed,
            current_version(),
            current_channel_label(),
            message,
        ),
    }
}

async fn verify_integrity_inner() -> Result<VerifyIntegrityResult, VerifyError> {
    let exe_path = std::env::current_exe()
        .map_err(|error| VerifyError::Local(format!("无法读取当前程序路径：{}", error)))?;
    let channel = detect_distribution_channel(&exe_path);
    let exe_bytes = std::fs::read(&exe_path)
        .map_err(|error| VerifyError::Local(format!("无法读取当前程序文件：{}", error)))?;

    let app_version = current_version();
    let signature_source = fetch_signature(&channel, &app_version).await?;
    verify_exe_signature(&exe_bytes, &signature_source.signature)?;

    Ok(build_result(
        true,
        VerifyIntegrityStatus::Verified,
        app_version.clone(),
        channel.label().to_string(),
        format!("当前为官方原版 v{}", app_version),
    ))
}

async fn fetch_signature(
    channel: &DistributionChannel,
    app_version: &str,
) -> Result<SignatureSource, VerifyError> {
    let asset_name = match channel {
        DistributionChannel::Installer => INSTALLER_EXE_SIGNATURE_ASSET,
        DistributionChannel::Portable => PORTABLE_EXE_SIGNATURE_ASSET,
    };
    fetch_signature_asset(app_version, asset_name).await
}

async fn fetch_signature_asset(
    app_version: &str,
    asset_name: &str,
) -> Result<SignatureSource, VerifyError> {
    let signature_url = release_asset_url(app_version, asset_name);
    let response = reqwest::Client::new()
        .get(&signature_url)
        .header("User-Agent", "LightC-integrity-check")
        .send()
        .await
        .map_err(|error| VerifyError::Network(error.to_string()))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(VerifyError::ReleaseUnavailable(format!(
            "当前版本 v{} 尚未发布官方签名资产 {}，请在 GitHub Release 完成后再校验。",
            app_version, asset_name
        )));
    }

    if !response.status().is_success() {
        return Err(VerifyError::Network(format!(
            "下载签名资产失败：HTTP {}",
            response.status()
        )));
    }

    let signature = response
        .text()
        .await
        .map_err(|error| VerifyError::Network(error.to_string()))?;

    Ok(SignatureSource { signature })
}

fn release_asset_url(app_version: &str, asset_name: &str) -> String {
    // 直接请求当前版本 Release 资产，避免 GitHub API 限流，也避免旧版误拿 latest 签名。
    format!(
        "https://github.com/Chunyu33/light-c/releases/download/v{}/{}",
        app_version, asset_name
    )
}

fn verify_exe_signature(exe_bytes: &[u8], signature_text: &str) -> Result<(), VerifyError> {
    let public_key_text = decode_base64_text(UPDATER_PUBLIC_KEY)
        .map_err(|message| VerifyError::Local(format!("官方公钥解析失败：{}", message)))?;
    let signature_text = decode_base64_text(signature_text).map_err(|message| {
        VerifyError::InvalidSignature(format!("签名文件解析失败：{}", message))
    })?;
    let public_key = PublicKey::decode(&public_key_text)
        .map_err(|error| VerifyError::Local(format!("官方公钥解析失败：{}", error)))?;
    let signature = Signature::decode(&signature_text)
        .map_err(|error| VerifyError::InvalidSignature(format!("签名文件解析失败：{}", error)))?;

    public_key
        .verify(exe_bytes, &signature, true)
        .map_err(|error| {
            VerifyError::InvalidSignature(format!("文件已被篡改或不是官方原版：{}", error))
        })
}

fn decode_base64_text(base64_text: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_text.trim())
        .map_err(|error| error.to_string())?;
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

fn detect_distribution_channel(exe_path: &Path) -> DistributionChannel {
    let marker_exists = exe_path
        .parent()
        .map(|parent| parent.join(PORTABLE_MARKER_FILE))
        .is_some_and(|marker_path| marker_path.is_file());

    if marker_exists {
        DistributionChannel::Portable
    } else {
        DistributionChannel::Installer
    }
}

fn current_channel_label() -> String {
    std::env::current_exe()
        .ok()
        .map(|path: PathBuf| detect_distribution_channel(&path).label().to_string())
        .unwrap_or_else(|| "未知版本".to_string())
}

fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn build_result(
    verified: bool,
    status: VerifyIntegrityStatus,
    version: String,
    channel: String,
    message: String,
) -> VerifyIntegrityResult {
    VerifyIntegrityResult {
        verified,
        status,
        version,
        channel,
        message,
        official_url: OFFICIAL_RELEASE_URL.to_string(),
    }
}

enum DistributionChannel {
    Installer,
    Portable,
}

impl DistributionChannel {
    fn label(&self) -> &'static str {
        match self {
            DistributionChannel::Installer => "安装版",
            DistributionChannel::Portable => "便携版",
        }
    }
}

enum VerifyError {
    Network(String),
    ReleaseUnavailable(String),
    InvalidSignature(String),
    Local(String),
}

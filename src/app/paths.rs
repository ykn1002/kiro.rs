//! 运行模式与配置/凭证路径解析。
//!
//! 服务器模式沿用相对路径（`config.json` / `credentials.json`），与 Docker 部署一致。
//! 桌面模式把文件落到系统数据目录，并在首次启动时写入默认模板，避免安装到只读目录后无处写入。

use std::path::PathBuf;

use crate::model::config::Config;
use crate::kiro::model::credentials::KiroCredentials;

use super::RunOptions;

/// 运行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    /// 服务器/Docker：相对路径，绑定 `config.host:port`
    Server,
    /// 桌面：系统数据目录，绑定 `127.0.0.1`，端口占用自动回退
    Desktop,
}

/// 解析后的实际文件路径。
pub struct ResolvedPaths {
    pub config_path: PathBuf,
    pub credentials_path: PathBuf,
}

/// 桌面模式数据目录：`~/Library/Application Support/kiro-rs`（macOS）、
/// `%APPDATA%\kiro-rs`（Windows）、`~/.config/kiro-rs`（Linux）。
///
/// 无法确定时回退到当前目录，保证不 panic。
pub fn desktop_data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "kiro-rs")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 按运行模式解析配置/凭证路径；显式传入的路径优先于模式默认值。
///
/// 桌面模式在解析后确保目录存在并写入缺失的默认模板文件。
pub fn resolved_paths(opts: &RunOptions) -> ResolvedPaths {
    match opts.mode {
        RuntimeMode::Server => ResolvedPaths {
            config_path: opts
                .config_path
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(Config::default_config_path())),
            credentials_path: opts
                .credentials_path
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(KiroCredentials::default_credentials_path())),
        },
        RuntimeMode::Desktop => {
            let dir = desktop_data_dir();
            let config_path = opts
                .config_path
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| dir.join("config.json"));
            let credentials_path = opts
                .credentials_path
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| dir.join("credentials.json"));
            bootstrap_desktop_files(&dir, &config_path, &credentials_path);
            ResolvedPaths {
                config_path,
                credentials_path,
            }
        }
    }
}

/// 桌面模式：确保数据目录存在并写入缺失的默认文件。
///
/// 已存在的文件绝不覆盖。config 默认模板生成随机 `apiKey` 与 `adminApiKey`，
/// 使桌面版开箱即用且 Admin UI 可自动认证。credentials 默认写空数组，
/// 用户从 Admin UI 添加。
fn bootstrap_desktop_files(dir: &PathBuf, config_path: &PathBuf, credentials_path: &PathBuf) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("创建桌面数据目录失败: {} ({})", e, dir.display());
        return;
    }

    if !config_path.exists() {
        match build_default_config() {
            Ok(json) => match std::fs::write(config_path, json) {
                Ok(_) => tracing::info!("已写入默认配置: {}", config_path.display()),
                Err(e) => tracing::warn!("写入默认配置失败: {} ({})", e, config_path.display()),
            },
            Err(e) => tracing::warn!("生成默认配置失败: {}", e),
        }
    }

    if !credentials_path.exists() {
        match std::fs::write(credentials_path, "[]\n") {
            Ok(_) => tracing::info!("已写入空凭证文件: {}", credentials_path.display()),
            Err(e) => tracing::warn!("写入凭证文件失败: {} ({})", e, credentials_path.display()),
        }
    }
}

/// 编译时嵌入的配置基底（仓库根的 config.example.json）。
/// 桌面版首次启动以它为默认配置，仅覆盖两个密钥与自动探测的版本。
const CONFIG_TEMPLATE: &str = include_str!("../../config.example.json");

/// 生成随机 hex token（32 位），用于桌面模式默认密钥。
fn random_token() -> String {
    let mut s = String::with_capacity(32);
    for _ in 0..32 {
        s.push(char::from_digit(fastrand::u32(0..16), 16).unwrap_or('0'));
    }
    s
}

/// 构建桌面模式首次默认配置：以 config.example.json 为基底，
/// 覆盖 `apiKey`（sk- 前缀随机）、`adminApiKey`（随机），
/// 并用本机探测到的 `systemVersion` / `kiroVersion` 覆盖（探测不到则保留模板值）。
fn build_default_config() -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(CONFIG_TEMPLATE).map_err(|e| format!("解析内置配置模板失败: {e}"))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| "内置配置模板根节点不是对象".to_string())?;

    // 密钥每次首启随机生成
    obj.insert(
        "apiKey".to_string(),
        serde_json::json!(format!("sk-{}", random_token())),
    );
    obj.insert("adminApiKey".to_string(), serde_json::json!(random_token()));

    // 版本自动探测，成功则覆盖模板里的值
    if let Some(v) = super::host_info::system_version() {
        tracing::info!("自动获取 systemVersion: {}", v);
        obj.insert("systemVersion".to_string(), serde_json::json!(v));
    }
    if let Some(v) = super::host_info::kiro_version() {
        tracing::info!("自动获取 kiroVersion: {}", v);
        obj.insert("kiroVersion".to_string(), serde_json::json!(v));
    }

    let mut out =
        serde_json::to_string_pretty(&value).map_err(|e| format!("序列化配置失败: {e}"))?;
    out.push('\n');
    Ok(out)
}

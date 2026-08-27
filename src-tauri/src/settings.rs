//! 桌面版本地设置。
//!
//! 与后端配置同目录（`desktop_data_dir()`），独立存放窗口/启动相关偏好，
//! 不与 `config.json` 混用（后者是服务端配置，可能被 Admin API 改写）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "desktop-settings.json";

/// 桌面偏好。字段用 serde 默认值，缺失字段向后兼容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopSettings {
    /// 静默启动：开机自启拉起时不弹窗，仅驻留托盘。默认关闭。
    /// 仅影响开机自启路径；手动打开始终显示窗口。
    #[serde(default)]
    pub silent_start: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self { silent_start: false }
    }
}

impl DesktopSettings {
    fn path() -> PathBuf {
        kiro_rs::app::desktop_data_dir().join(SETTINGS_FILE)
    }

    /// 读取设置；文件不存在或解析失败时回退到默认值（不报错）。
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("解析桌面设置失败，使用默认值: {}", e);
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// 写回设置。目录缺失时尝试创建。
    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&path, s) {
                    tracing::error!("写入桌面设置失败: {} ({})", e, path.display());
                }
            }
            Err(e) => tracing::error!("序列化桌面设置失败: {}", e),
        }
    }
}

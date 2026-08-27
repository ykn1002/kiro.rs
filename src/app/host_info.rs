//! 本机环境探测：为桌面版自动获取真实的 `systemVersion` 与 `kiroVersion`。
//!
//! 仅用于桌面模式首次生成默认配置时填充真实值，探测失败则返回 `None`，
//! 由调用方回退到 [`crate::model::config`] 的内置默认值。
//!
//! - `systemVersion`：格式 `平台#内核版本`，如 `darwin#24.6.0` / `win32#10.0.22631`。
//! - `kiroVersion`：本机安装的 Kiro IDE 版本（macOS 读 Info.plist；Windows 读安装目录）。

use std::process::Command;

/// 探测本机 `systemVersion`（`平台#内核版本`）。失败返回 `None`。
pub fn system_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let release = uname_release()?;
        Some(format!("darwin#{release}"))
    }
    #[cfg(target_os = "windows")]
    {
        let build = windows_os_build()?;
        Some(format!("win32#{build}"))
    }
    #[cfg(target_os = "linux")]
    {
        let release = uname_release()?;
        Some(format!("linux#{release}"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        None
    }
}

/// 探测本机安装的 Kiro IDE 版本。失败返回 `None`。
pub fn kiro_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        macos_kiro_version()
    }
    #[cfg(target_os = "windows")]
    {
        windows_kiro_version()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

// ---- 内核版本（macOS / Linux）----
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn uname_release() -> Option<String> {
    let out = Command::new("uname").arg("-r").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

// ---- Windows 构建号 ----
#[cfg(target_os = "windows")]
fn windows_os_build() -> Option<String> {
    // `cmd /c ver` 输出形如：Microsoft Windows [Version 10.0.22631.4460]
    let out = Command::new("cmd").args(["/c", "ver"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let start = s.find("Version ")? + "Version ".len();
    let rest = &s[start..];
    let end = rest.find(']')?;
    let full = &rest[..end]; // 如 10.0.22631.4460
    // 取前三段 major.minor.build，与 Kiro/VSCode 上报格式一致
    let parts: Vec<&str> = full.split('.').collect();
    if parts.len() >= 3 {
        Some(format!("{}.{}.{}", parts[0], parts[1], parts[2]))
    } else if !full.is_empty() {
        Some(full.to_string())
    } else {
        None
    }
}

// ---- Kiro 版本：macOS ----
#[cfg(target_os = "macos")]
fn macos_kiro_version() -> Option<String> {
    // 常见安装位置：系统级与用户级
    let candidates = [
        "/Applications/Kiro.app/Contents/Info.plist".to_string(),
        format!(
            "{}/Applications/Kiro.app/Contents/Info.plist",
            std::env::var("HOME").unwrap_or_default()
        ),
    ];
    for plist in candidates.iter().filter(|p| std::path::Path::new(p).exists()) {
        let out = Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleShortVersionString", plist])
            .output()
            .ok()?;
        if out.status.success() {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

// ---- Kiro 版本：Windows ----
#[cfg(target_os = "windows")]
fn windows_kiro_version() -> Option<String> {
    // Kiro 为 Code OSS fork，版本号在安装目录 resources/app/product.json 的 "version" 字段。
    // 官方用户级安装路径为 %LOCALAPPDATA%\Programs\kiro\resources\app\product.json（小写 kiro）；
    // 另尝试系统级 Program Files 作为回退。
    let mut roots: Vec<String> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        roots.push(format!("{local}\\Programs\\kiro\\resources\\app\\product.json"));
        // 大小写回退（安装器版本差异）
        roots.push(format!("{local}\\Programs\\Kiro\\resources\\app\\product.json"));
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        roots.push(format!("{pf}\\kiro\\resources\\app\\product.json"));
        roots.push(format!("{pf}\\Kiro\\resources\\app\\product.json"));
    }
    for path in roots.iter().filter(|p| std::path::Path::new(p).exists()) {
        if let Ok(content) = std::fs::read_to_string(path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(ver) = v.get("version").and_then(|x| x.as_str())
            && !ver.is_empty()
        {
            return Some(ver.to_string());
        }
    }
    None
}

//! 本机环境探测：从本机系统与已安装的 Kiro IDE 读取上游版本指纹。
//!
//! 用于桌面模式首次生成默认配置，以及 Admin「从本机 Kiro 读取」一键填充。
//! 探测失败的字段返回 `None`，由调用方回退到已有值。
//!
//! - `systemVersion`：格式 `平台#内核版本`，如 `darwin#24.6.0` / `win32#10.0.22631`。
//! - `kiroVersion`：本机安装的 Kiro IDE 版本（macOS 读 Info.plist；Windows 读安装目录）。
//! - `nodeVersion`：Kiro 内置 Electron 的 Node 版本（`ELECTRON_RUN_AS_NODE=1` 执行 Kiro 主程序）。
//! - `streamingSdkVersion`：kiro-agent 扩展打包的 `@aws/codewhisperer-streaming-client` 版本。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

/// 本机探测到的版本指纹，探测不到的字段为 `None`
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedFingerprint {
    pub kiro_version: Option<String>,
    pub system_version: Option<String>,
    pub node_version: Option<String>,
    pub streaming_sdk_version: Option<String>,
}

/// 一次性探测全部版本指纹字段（含子进程与读大文件，调用方应放到阻塞线程执行）
pub fn detect_fingerprint() -> DetectedFingerprint {
    let fp = DetectedFingerprint {
        kiro_version: kiro_version(),
        system_version: system_version(),
        node_version: node_version(),
        streaming_sdk_version: streaming_sdk_version(),
    };
    tracing::info!(
        "本机版本指纹探测结果: kiroVersion={:?}, systemVersion={:?}, nodeVersion={:?}, streamingSdkVersion={:?}",
        fp.kiro_version,
        fp.system_version,
        fp.node_version,
        fp.streaming_sdk_version
    );
    fp
}

/// 本机 Kiro 安装根目录候选（macOS 为 `Kiro.app/Contents`，Windows 为安装目录），只返回存在的
fn kiro_install_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        roots.push(PathBuf::from("/Applications/Kiro.app/Contents"));
        if let Ok(home) = std::env::var("HOME") {
            roots.push(PathBuf::from(home).join("Applications/Kiro.app/Contents"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        // 官方用户级安装为 %LOCALAPPDATA%\Programs\kiro；Windows 路径大小写不敏感，无需重复大写变体
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Programs").join("kiro"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            roots.push(PathBuf::from(pf).join("kiro"));
        }
    }
    roots.retain(|p| p.is_dir());
    roots
}

/// 安装根目录下的 `resources/app` 目录（macOS 为 `Resources/app`）
fn app_dir(root: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        root.join("Resources").join("app")
    } else {
        root.join("resources").join("app")
    }
}

/// 安装根目录下的 Kiro 主程序
fn kiro_executable(root: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        root.join("MacOS").join("Kiro")
    } else {
        root.join("Kiro.exe")
    }
}

/// 探测 Kiro 内置 Electron 的 Node 版本。失败返回 `None`。
pub fn node_version() -> Option<String> {
    for root in kiro_install_roots() {
        let exe = kiro_executable(&root);
        if !exe.is_file() {
            continue;
        }
        let out = Command::new(&exe)
            .env("ELECTRON_RUN_AS_NODE", "1")
            .args(["-p", "process.versions.node"])
            .output();
        match out {
            Ok(out) if out.status.success() => {
                let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if is_version_like(&v) {
                    return Some(v);
                }
                tracing::warn!("读取 Kiro Node 版本输出异常: {:?} ({})", v, exe.display());
            }
            Ok(out) => tracing::warn!(
                "读取 Kiro Node 版本失败: status={} ({})",
                out.status,
                exe.display()
            ),
            Err(e) => tracing::warn!("启动 Kiro 读取 Node 版本失败: {} ({})", e, exe.display()),
        }
    }
    None
}

/// 探测 kiro-agent 扩展里打包的 `@aws/codewhisperer-streaming-client` 版本。失败返回 `None`。
pub fn streaming_sdk_version() -> Option<String> {
    const MARKER: &str = "name:\"@aws/codewhisperer-streaming-client\"";
    for root in kiro_install_roots() {
        let bundle = app_dir(&root)
            .join("extensions")
            .join("kiro.kiro-agent")
            .join("dist")
            .join("extension.js");
        let Ok(content) = std::fs::read_to_string(&bundle) else {
            continue;
        };
        if let Some(v) = extract_bundled_version(&content, MARKER) {
            return Some(v);
        }
        tracing::warn!("extension.js 中未找到 streaming SDK 版本 ({})", bundle.display());
    }
    None
}

/// 在打包后的 JS 中定位 `name:"<pkg>"`，取其后紧邻的 `version:"x.y.z"`
fn extract_bundled_version(content: &str, marker: &str) -> Option<String> {
    let start = content.find(marker)? + marker.len();
    // package.json 内联对象里 version 紧随 name/description，限定窗口避免误取后续包的版本
    let window = &content[start..content.len().min(start + 1024)];
    let vstart = window.find("version:\"")? + "version:\"".len();
    let rest = &window[vstart..];
    let v = &rest[..rest.find('"')?];
    is_version_like(v).then(|| v.to_string())
}

/// 粗校验 `数字.数字...` 形式的版本号
fn is_version_like(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').count() >= 2
        && s.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

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
    for plist in candidates
        .iter()
        .filter(|p| std::path::Path::new(p).exists())
    {
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
        roots.push(format!(
            "{local}\\Programs\\kiro\\resources\\app\\product.json"
        ));
        // 大小写回退（安装器版本差异）
        roots.push(format!(
            "{local}\\Programs\\Kiro\\resources\\app\\product.json"
        ));
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

#[cfg(test)]
mod tests {
    use super::{extract_bundled_version, is_version_like};

    const MARKER: &str = "name:\"@aws/codewhisperer-streaming-client\"";

    #[test]
    fn extracts_version_after_marker() {
        let js = r#"var a=R((x,y)=>{y.exports={name:"@aws/codewhisperer-streaming-client",description:"@aws/codewhisperer-streaming-client",version:"1.0.39",main:"./dist"}})"#;
        assert_eq!(extract_bundled_version(js, MARKER).as_deref(), Some("1.0.39"));
    }

    #[test]
    fn missing_marker_returns_none() {
        let js = r#"{name:"@aws-sdk/client-sso-oidc",version:"3.980.0"}"#;
        assert_eq!(extract_bundled_version(js, MARKER), None);
    }

    #[test]
    fn version_too_far_from_marker_is_ignored() {
        let js = format!(
            "{{name:\"@aws/codewhisperer-streaming-client\",{}version:\"9.9.9\"}}",
            "x".repeat(2000)
        );
        assert_eq!(extract_bundled_version(&js, MARKER), None);
    }

    #[test]
    fn version_like_check() {
        assert!(is_version_like("24.21.0"));
        assert!(is_version_like("1.0"));
        assert!(!is_version_like("24"));
        assert!(!is_version_like("v24.1.0"));
        assert!(!is_version_like("1..0"));
        assert!(!is_version_like(""));
    }
}

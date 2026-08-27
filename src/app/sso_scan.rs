//! 扫描本机 AWS SSO 缓存目录，自动发现可导入的 Kiro IdC 凭证。
//!
//! 目录：`~/.aws/sso/cache`（macOS/Linux）、`%USERPROFILE%\.aws\sso\cache`（Windows）。
//! 其中：
//! - `kiro-auth-token*.json`：含 `refreshToken` / `region` / `authMethod` / `clientIdHash`
//! - `<clientIdHash>.json`：OIDC 客户端注册，含 `clientId` / `clientSecret`
//!
//! 两者通过 token 文件里的 `clientIdHash`（等于客户端注册文件名去掉 .json）关联。
//! 扫描仅读取、组装候选，不做验活（验活交给导入时的 add_credential 流程）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 扫描到的一条可导入凭证候选。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoCredentialCandidate {
    pub refresh_token: String,
    pub client_id: String,
    pub client_secret: String,
    /// 认证区域（token 文件里的 region），可能为空
    pub region: Option<String>,
    /// 来源 token 文件名（供 UI 展示区分多个账号）
    pub source_file: String,
}

#[derive(Deserialize)]
struct KiroAuthToken {
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "clientIdHash")]
    client_id_hash: Option<String>,
    region: Option<String>,
}

#[derive(Deserialize)]
struct ClientRegistration {
    #[serde(rename = "clientId")]
    client_id: Option<String>,
    #[serde(rename = "clientSecret")]
    client_secret: Option<String>,
}

/// AWS SSO 缓存目录。取不到家目录返回 `None`。
pub fn sso_cache_dir() -> Option<PathBuf> {
    let home = home_dir()?;
    Some(home.join(".aws").join("sso").join("cache"))
}

#[cfg(target_os = "windows")]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(not(target_os = "windows"))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// 扫描缓存目录，返回所有可组装的凭证候选。
///
/// 目录不存在或无匹配文件时返回空 Vec（非错误）。单个文件解析失败会被跳过。
pub fn scan_sso_credentials() -> Vec<SsoCredentialCandidate> {
    let dir = match sso_cache_dir() {
        Some(d) if d.is_dir() => d,
        _ => return Vec::new(),
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("读取 SSO 缓存目录失败: {} ({})", e, dir.display());
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // 只处理 kiro-auth-token*.json（Kiro 写的 token 缓存）
        if !(name.starts_with("kiro-auth-token") && name.ends_with(".json")) {
            continue;
        }
        match assemble_candidate(&dir, &path, &name) {
            Ok(Some(c)) => out.push(c),
            Ok(None) => {}
            Err(e) => tracing::warn!("解析 SSO token 文件 {} 失败: {}", name, e),
        }
    }
    out
}

/// 从一个 token 文件组装候选：读 refreshToken/clientIdHash，再读对应客户端注册文件。
fn assemble_candidate(
    dir: &std::path::Path,
    token_path: &std::path::Path,
    source_file: &str,
) -> Result<Option<SsoCredentialCandidate>, String> {
    let token_raw = std::fs::read_to_string(token_path).map_err(|e| e.to_string())?;
    let token: KiroAuthToken =
        serde_json::from_str(&token_raw).map_err(|e| format!("token JSON 非法: {e}"))?;

    let refresh_token = match token.refresh_token.map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None), // 无 refreshToken，跳过
    };
    let client_hash = match token.client_id_hash.map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None), // 无 clientIdHash，无法关联客户端注册文件
    };

    // 客户端注册文件：<clientIdHash>.json，与 token 同目录
    let client_path = dir.join(format!("{client_hash}.json"));
    if !client_path.is_file() {
        return Ok(None);
    }
    let client_raw = std::fs::read_to_string(&client_path).map_err(|e| e.to_string())?;
    let client: ClientRegistration =
        serde_json::from_str(&client_raw).map_err(|e| format!("客户端 JSON 非法: {e}"))?;

    let client_id = client.client_id.unwrap_or_default().trim().to_string();
    let client_secret = client.client_secret.unwrap_or_default().trim().to_string();
    if client_id.is_empty() || client_secret.is_empty() {
        return Ok(None);
    }

    Ok(Some(SsoCredentialCandidate {
        refresh_token,
        client_id,
        client_secret,
        region: token.region.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        source_file: source_file.to_string(),
    }))
}

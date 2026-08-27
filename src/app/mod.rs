//! 服务启动装配。
//!
//! 把原先散落在 `main.rs` 的「加载配置 → 构建路由 → 绑定监听」流程抽成可复用函数，
//! 既供二进制 `main.rs` 调用，也供桌面版（Tauri 壳）在自带 runtime 中调用。
//!
//! 两种运行模式（[`RuntimeMode`]）：
//! - [`RuntimeMode::Server`]：默认，含 Docker 部署。保持相对路径与 `config.host:port` 绑定，
//!   行为与重构前完全一致。
//! - [`RuntimeMode::Desktop`]：配置/凭证/统计落到系统数据目录，监听 `127.0.0.1`，
//!   端口被占用时自动选空闲端口，失败不 panic。

mod host_info;
mod paths;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;

use crate::admin;
use crate::admin_ui;
use crate::anthropic;
use crate::http_client;
use crate::kiro::endpoint::{IdeEndpoint, KiroEndpoint};
use crate::kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::MultiTokenManager;
use crate::metrics;
use crate::model::config::Config;
use crate::model_stats;
use crate::openai;
use crate::stats_db;
use crate::token;

pub use paths::{RuntimeMode, desktop_data_dir, resolved_paths};

/// 服务启动错误。桌面模式据此向用户展示错误而非直接退出进程。
#[derive(Debug)]
pub enum StartupError {
    /// 配置文件加载失败
    Config(String),
    /// 凭证文件加载失败
    Credentials(String),
    /// 配置缺少 apiKey
    MissingApiKey,
    /// 默认端点未注册
    UnknownDefaultEndpoint(String),
    /// 凭据指定了未知端点
    UnknownCredentialEndpoint { id: Option<u64>, endpoint: String },
    /// 创建 Token 管理器失败
    TokenManager(String),
    /// 绑定监听地址失败
    Bind(String),
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupError::Config(e) => write!(f, "加载配置失败: {e}"),
            StartupError::Credentials(e) => write!(f, "加载凭证失败: {e}"),
            StartupError::MissingApiKey => write!(f, "配置文件中未设置 apiKey"),
            StartupError::UnknownDefaultEndpoint(name) => write!(f, "默认端点 \"{name}\" 未注册"),
            StartupError::UnknownCredentialEndpoint { id, endpoint } => {
                write!(f, "凭据 id={id:?} 指定了未知端点 \"{endpoint}\"")
            }
            StartupError::TokenManager(e) => write!(f, "创建 Token 管理器失败: {e}"),
            StartupError::Bind(e) => write!(f, "绑定监听地址失败: {e}"),
        }
    }
}

impl std::error::Error for StartupError {}

/// 启动选项。由 CLI 参数或桌面壳构造。
pub struct RunOptions {
    /// 运行模式，决定默认路径解析与监听地址策略
    pub mode: RuntimeMode,
    /// 配置文件路径。`None` 时按运行模式取默认路径
    pub config_path: Option<String>,
    /// 凭证文件路径。`None` 时按运行模式取默认路径
    pub credentials_path: Option<String>,
}

/// 已完成装配、绑定好监听端口、等待 `serve` 的服务。
pub struct Server {
    app: Router,
    listener: TcpListener,
    /// 实际监听端口（桌面模式端口可能因占用而自动变更，需回传给窗口层）
    local_port: u16,
    /// 配置中期望的端口（`config.port`）。与 `local_port` 不一致即表示发生了端口冲突回退
    requested_port: u16,
    /// 生效的 Admin API Key（若已启用）。桌面壳据此向 WebView 注入 localStorage 实现免登录
    admin_api_key: Option<String>,
}

impl Server {
    /// 实际监听端口。
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// 配置中期望的端口。
    pub fn requested_port(&self) -> u16 {
        self.requested_port
    }

    /// 是否发生了端口冲突（期望端口被占用，实际回退到了其他端口）。
    pub fn port_conflicted(&self) -> bool {
        self.requested_port != self.local_port
    }

    /// 生效的 Admin API Key（若 Admin API 已启用）。
    pub fn admin_api_key(&self) -> Option<&str> {
        self.admin_api_key.as_deref()
    }

    /// 阻塞运行 HTTP 服务直到进程结束。
    pub async fn serve(self) -> anyhow::Result<()> {
        axum::serve(self.listener, self.app).await?;
        Ok(())
    }
}

/// 装配服务：加载配置/凭证、构建路由、绑定监听端口。
///
/// 不启动 accept 循环；调用方拿到 [`Server`] 后自行决定何时 `serve`（桌面壳需先取端口）。
pub async fn build(opts: RunOptions) -> Result<Server, StartupError> {
    metrics::init_start_time();

    let paths = resolved_paths(&opts);

    // 加载配置
    let mut config = Config::load(&paths.config_path)
        .map_err(|e| StartupError::Config(format!("{} ({})", e, paths.config_path.display())))?;
    let requested_tls = config.tls_backend;
    config.tls_backend = http_client::effective_tls_backend(requested_tls);
    http_client::warn_tls_backend_fallback(requested_tls, config.tls_backend);

    // 初始化全局模型注册表，须在任何请求进入前完成
    anthropic::init_model_mapping(
        config.effective_models(),
        config.effective_model_aliases(),
        config.default_model.clone(),
    );

    // 分块写入策略（默认关闭）
    anthropic::set_chunked_write_policy(config.chunked_write_policy.clone());
    if config.chunked_write_policy.enabled {
        tracing::info!(
            trigger_lines = config.chunked_write_policy.trigger_lines,
            chunk_lines = config.chunked_write_policy.chunk_lines,
            "已启用 Write/Edit 分块写入策略注入（会增加请求次数与配额消耗）"
        );
    }

    // codex 工具参数截断纠正开关（默认开）
    openai::set_codex_truncation_correction(config.codex_truncation_correction);
    if !config.codex_truncation_correction {
        tracing::info!("codex 截断纠正文本已关闭（挂空 item 封口仍生效）");
    }

    if config.default_model.is_some() || !config.model_aliases.is_empty() {
        tracing::info!(
            default_model = ?config.default_model,
            alias_count = config.model_aliases.len(),
            "已加载 OpenAI/Codex 模型映射"
        );
    }

    // 加载凭证（支持单对象或数组格式）
    let credentials_path_str = paths.credentials_path.to_string_lossy().to_string();
    let credentials_config = CredentialsConfig::load(&paths.credentials_path).map_err(|e| {
        StartupError::Credentials(format!("{} ({})", e, paths.credentials_path.display()))
    })?;

    let is_multiple_format = credentials_config.is_multiple();
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // KIRO_API_KEY 环境变量：自动创建最高优先级 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!("主凭证: {:?}", first_credentials);

    // API Key
    let api_key = config.api_key.clone().ok_or(StartupError::MissingApiKey)?;

    let shared_api_key: anthropic::SharedApiKey =
        Arc::new(parking_lot::RwLock::new(api_key.clone()));

    // 代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
    }

    if !endpoints.contains_key(&config.default_endpoint) {
        return Err(StartupError::UnknownDefaultEndpoint(
            config.default_endpoint.clone(),
        ));
    }

    for cred in &credentials_list {
        let name = cred.endpoint.as_deref().unwrap_or(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            return Err(StartupError::UnknownCredentialEndpoint {
                id: cred.id,
                endpoint: name.to_string(),
            });
        }
    }

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

    // MultiTokenManager 与 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(PathBuf::from(&credentials_path_str)),
        is_multiple_format,
    )
    .map_err(|e| StartupError::TokenManager(e.to_string()))?;
    let token_manager = Arc::new(token_manager);

    // 模型统计与监控时间序列落盘（与凭证同目录）
    model_stats::global().init_path(
        token_manager
            .cache_dir()
            .map(|d| d.join("kiro_model_stats.json")),
    );
    stats_db::init(
        token_manager
            .cache_dir()
            .map(|d| d.join("kiro_usage_stats.db")),
    );

    let kiro_provider = KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
    );

    // count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
    });

    // Anthropic API 路由
    let anthropic_app = anthropic::create_router_with_provider(
        shared_api_key.clone(),
        Some(kiro_provider),
        config.extract_thinking,
        config.passthrough_retry_after,
    );

    // Admin API 路由（仅当配置了非空 admin_api_key）
    let admin_key_valid = config
        .admin_api_key
        .as_ref()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);

    let mut effective_admin_key: Option<String> = None;
    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app
        } else {
            effective_admin_key = Some(admin_key.clone());
            // Admin API Key 共享句柄：AdminState 认证与 AdminService 修改共用，实现热替换
            let shared_admin_api_key: anthropic::SharedApiKey =
                Arc::new(parking_lot::RwLock::new(admin_key.clone()));
            let admin_service = admin::AdminService::new(
                token_manager.clone(),
                endpoint_names.clone(),
                shared_api_key.clone(),
                shared_admin_api_key.clone(),
            );
            let admin_state = admin::AdminState::new(shared_admin_api_key, admin_service);
            let admin_app = admin::create_admin_router(admin_state);
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .nest("/admin", admin_ui_app)
        }
    } else {
        anthropic_app
    };

    // 绑定监听地址
    let listener = bind_listener(&opts.mode, &config)
        .await
        .map_err(|e| StartupError::Bind(e.to_string()))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| StartupError::Bind(e.to_string()))?;
    let local_port = local_addr.port();

    tracing::info!("启动 API 端点: {}", local_addr);
    tracing::info!("API Key: {}***", &api_key[..(api_key.len() / 2)]);
    tracing::info!("可用 API:");
    tracing::info!("  GET  /metrics");
    tracing::info!("  GET  /healthz");
    tracing::info!("  GET  /readyz");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    tracing::info!("  POST /v1/chat/completions  (OpenAI Chat Completions)");
    tracing::info!("  POST /v1/responses         (OpenAI Responses / Codex 原生)");
    if admin_key_valid {
        tracing::info!("Admin API:");
        tracing::info!("  GET  /api/admin/credentials");
        tracing::info!("Admin UI:");
        tracing::info!("  GET  /admin");
    }

    Ok(Server {
        app,
        listener,
        local_port,
        requested_port: config.port,
        admin_api_key: effective_admin_key,
    })
}

/// 便捷入口：装配并阻塞运行。二进制 `main.rs` 使用。
pub async fn run(opts: RunOptions) -> anyhow::Result<()> {
    let server = build(opts).await?;
    server.serve().await
}

/// 探测某端口是否空闲（用于冲突检测）。
///
/// 故意绑定到 `0.0.0.0`（所有地址）而非 `127.0.0.1`：桌面版实际只监听环回，
/// 但 Docker、其他容器/服务常以 `*`/`0.0.0.0` 通配监听。若只探测环回，
/// 一个绑在 `*:port` 的进程不会与 `127.0.0.1:port` 冲突，会被漏报为“可用”。
/// 用 `0.0.0.0` 探测能与这类通配监听真正冲突，从而如实发现端口被占用。
///
/// 仅做一次性尝试绑定并立即释放，不保证之后仍空闲（TOCTOU）。
/// 端口 0 视为无效（表示随机分配，不应作为期望值）。
pub fn is_port_available(port: u16) -> bool {
    if port == 0 {
        return false;
    }
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// 桌面模式下读取当前配置文件里的期望端口（`config.port`）。
///
/// 直接解析 JSON 的 `port` 字段，缺失时回退到默认端口，读不到文件也回退。
pub fn desktop_configured_port() -> u16 {
    let path = paths::desktop_data_dir().join("config.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(8080)
}

/// 桌面模式下把新端口写回 `config.json` 的 `port` 字段，保留其余所有字段。
///
/// 采用「读 → 改单字段 → 写回」而非序列化整个 `Config`，避免覆盖用户手改的其它键。
/// 端口变更需重启应用后由 `bind_listener` 重新绑定才生效。
pub fn desktop_set_configured_port(port: u16) -> Result<(), String> {
    if port == 0 {
        return Err("端口不能为 0".to_string());
    }
    let path = paths::desktop_data_dir().join("config.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取配置失败: {e} ({})", path.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("解析配置失败: {e}"))?;
    match value.as_object_mut() {
        Some(obj) => {
            obj.insert("port".to_string(), serde_json::json!(port));
        }
        None => return Err("配置文件根节点不是对象".to_string()),
    }
    let serialized =
        serde_json::to_string_pretty(&value).map_err(|e| format!("序列化配置失败: {e}"))?;
    std::fs::write(&path, serialized + "\n")
        .map_err(|e| format!("写入配置失败: {e} ({})", path.display()))?;
    Ok(())
}

/// 校验一段 JSON 文本是否为可用的桌面配置：能反序列化为 [`Config`]，
/// 且带有非空 `apiKey` 与 `adminApiKey`。
///
/// 这两个字段对桌面版是硬性要求：缺 `apiKey` 会导致 [`build`] 直接失败；
/// `adminApiKey` 为空会关闭 Admin API，进而使 Admin UI 免登录与导入入口失效。
/// 校验通过返回美化后的 JSON 文本（用于写回，规范缩进）。
fn validate_desktop_config_json(content: &str) -> Result<String, String> {
    let config: Config =
        serde_json::from_str(content).map_err(|e| format!("配置格式非法，无法解析: {e}"))?;
    if config.api_key.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Err("导入的配置缺少 apiKey，桌面版无法启动".to_string());
    }
    if config
        .admin_api_key
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err("导入的配置缺少 adminApiKey，将导致管理界面无法访问".to_string());
    }
    // 用解析回来的 value 重新美化（去掉可能的注释/多余空白，规范化）
    let value: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("配置格式非法: {e}"))?;
    serde_json::to_string_pretty(&value).map_err(|e| format!("序列化失败: {e}"))
}

/// 桌面模式下从给定文件路径导入完整 `config.json`（整体覆盖）。
///
/// 先校验（见 [`validate_desktop_config_json`]）再覆盖写入桌面配置文件，
/// 避免写入一个启动不了的配置。导入后需重启应用才生效。
/// 返回导入源文件里声明的端口（供前端提示）。
pub fn desktop_import_config(source_path: &str) -> Result<u16, String> {
    let content = std::fs::read_to_string(source_path)
        .map_err(|e| format!("读取导入文件失败: {e} ({source_path})"))?;
    let normalized = validate_desktop_config_json(&content)?;

    // 解析端口用于回传提示（缺失则按默认 8080）
    let port = serde_json::from_str::<serde_json::Value>(&normalized)
        .ok()
        .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(8080);

    let dir = paths::desktop_data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
    let dest = dir.join("config.json");
    std::fs::write(&dest, normalized + "\n")
        .map_err(|e| format!("写入配置失败: {e} ({})", dest.display()))?;
    Ok(port)
}

/// 根据运行模式绑定监听地址。
///
/// - Server：`config.host:config.port`，与重构前一致（端口占用即报错）。
/// - Desktop：监听 `127.0.0.1:config.port`；若期望端口被占用（含 Docker 等 `*`/`0.0.0.0`
///   通配监听，见 [`is_port_available`]），主动回退到系统分配的空闲端口（`:0`）。
///   主动回退使 `requested_port != local_port`，从而如实标记端口冲突；同时避免与
///   通配监听在同一端口“共存”导致客户端连接落到谁不确定。
async fn bind_listener(mode: &RuntimeMode, config: &Config) -> std::io::Result<TcpListener> {
    match mode {
        RuntimeMode::Server => {
            let addr = format!("{}:{}", config.host, config.port);
            TcpListener::bind(&addr).await
        }
        RuntimeMode::Desktop => {
            // 先用 0.0.0.0 探测：能发现仅绑环回时探不到的通配占用（如 Docker）。
            if !is_port_available(config.port) {
                tracing::warn!(
                    "桌面模式期望端口 {} 已被占用（可能是 Docker 等通配监听），改用系统分配的空闲端口",
                    config.port
                );
                return TcpListener::bind("127.0.0.1:0").await;
            }
            let preferred = format!("127.0.0.1:{}", config.port);
            match TcpListener::bind(&preferred).await {
                Ok(l) => Ok(l),
                Err(e) => {
                    tracing::warn!(
                        "桌面模式首选端口 {} 绑定失败（{}），改用系统分配的空闲端口",
                        config.port,
                        e
                    );
                    TcpListener::bind("127.0.0.1:0").await
                }
            }
        }
    }
}

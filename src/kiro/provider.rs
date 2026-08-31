//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::{Capability, KiroCredentials};
use crate::kiro::token_manager::{CredentialRpmExceeded, MultiTokenManager, RpmChargeMode};
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// 透传上游额度耗尽的响应标志（部分上游返回 `403 + INSUFFICIENT_BALANCE`）
const PASSTHROUGH_QUOTA_EXHAUSTED_MARKER: &str = "INSUFFICIENT_BALANCE";

/// 透传请求的入站协议
///
/// 决定 body 语义、目标路径（`/v1/messages` vs `/v1/responses`）和凭据选择能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassthroughProtocol {
    /// Anthropic `/v1/messages`
    Anthropic,
    /// OpenAI `/v1/responses`
    Openai,
}

impl PassthroughProtocol {
    /// 透传时拼接到 base_url 后的路径
    fn path(&self) -> &'static str {
        match self {
            PassthroughProtocol::Anthropic => "/v1/messages",
            PassthroughProtocol::Openai => "/v1/responses",
        }
    }
}

/// 一次上游请求的输入
///
/// 同时携带两份 body：选中 Kiro 凭据时用翻译后的 `kiro_body`，选中透传凭据时
/// 用原始入站字节 `raw_body`（不经翻译，原样转发给上游）。
pub struct UpstreamRequest<'a> {
    /// 翻译后的 Kiro 请求体（Kiro 凭据使用）
    pub kiro_body: &'a str,
    /// 原始入站字节（透传凭据使用）
    pub raw_body: &'a bytes::Bytes,
    /// 凭据选择能力（决定哪些凭据可参与本次请求）
    ///
    /// - `Anthropic`：可落到 Kiro 或 anthropic 透传
    /// - `Openai`：可落到 Kiro 或 openai 透传
    /// - `KiroOnly`：仅 Kiro（Chat Completions 等无对应透传协议的入站）
    pub capability: Capability,
    /// 仅透传：Kiro 翻译失败（模型名不在 Kiro 表）时的 fallback，排除 Kiro 凭据。
    /// 此时 `kiro_body` 通常为占位空串，只有透传凭据能被选中。
    pub passthrough_only: bool,
    /// 透传路径协议（仅当选中透传凭据时使用）
    pub protocol: PassthroughProtocol,
    /// 模型名提示（用于 RPM 凭据选择）
    pub model_hint: Option<&'a str>,
}

/// 一次上游请求的成功结果
pub struct UpstreamResponse {
    /// 上游 HTTP 响应
    pub response: reqwest::Response,
    /// 最终成功的凭据 id（供监控按凭据统计）
    pub credential_id: u64,
    /// 是否为透传凭据（true 时响应为标准协议 SSE，需原样转发，不做 Kiro 解码）
    pub passthrough: bool,
}

/// 上游 API 返回的 HTTP 错误，携带原始状态码以便上层按需透传给客户端。
///
/// provider 在重试耗尽后会把最后一次失败包装成本类型；`map_provider_error`
/// 据此对客户端可正确处理的状态码（如 429 限流、402 额度）做透传，
/// 其余状态码仍按 502 处理（不暴露上游凭据/权限细节给客户端）。
#[derive(Debug)]
pub(crate) struct UpstreamApiError {
    /// 上游返回的 HTTP 状态码
    pub status: u16,
    /// 用于日志/错误信息的完整描述
    pub message: String,
    /// 429 限流时建议客户端等待时长（本地 RPM 或上游 Retry-After）
    pub retry_after: Option<Duration>,
}

impl fmt::Display for UpstreamApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for UpstreamApiError {}

fn local_rpm_limit_error(rpm: &CredentialRpmExceeded) -> anyhow::Error {
    UpstreamApiError {
        status: 429,
        message: rpm.to_string(),
        retry_after: Some(rpm.retry_after),
    }
    .into()
}

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
const MAX_TOTAL_RETRIES: usize = 9;

fn record_request_failure_metrics(last_error: &Option<anyhow::Error>) {
    crate::metrics::inc_request_error();
    if let Some(e) = last_error {
        if e.downcast_ref::<UpstreamApiError>()
            .is_some_and(|a| a.status == 429)
        {
            crate::metrics::inc_upstream_rate_limited();
        }
    }
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
}

impl KiroProvider {
    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client
        let initial_client = build_client(proxy.as_ref(), 720, tls_backend).unwrap_or_else(|e| {
            panic!("创建 HTTP 客户端失败: {e}");
        });
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), initial_client);

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint,
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }
        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 构建透传请求：原样转发原始入站字节到第三方上游
    fn build_passthrough_request(
        &self,
        credentials: &KiroCredentials,
        protocol: PassthroughProtocol,
        raw_body: &bytes::Bytes,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let base_url = credentials
            .base_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("透传凭据缺少 baseUrl"))?;
        let api_key = credentials
            .upstream_api_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("透传凭据缺少 upstreamApiKey"))?;

        let url = format!("{}{}", base_url.trim_end_matches('/'), protocol.path());

        let request = self
            .client_for(credentials)?
            .post(&url)
            .body(raw_body.clone())
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("Authorization", format!("Bearer {}", api_key));
        Ok(request)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）
    /// 获取可用凭据数量（未禁用）
    pub fn available_credentials(&self) -> usize {
        self.token_manager.available_count()
    }

    /// 是否存在能服务指定协议的透传凭据（handler Kiro 翻译失败时判断能否 fallback）
    pub fn has_passthrough_credential(&self, cap: Capability) -> bool {
        self.token_manager.has_passthrough_credential(cap)
    }

    /// 获取凭据总数
    pub fn total_credentials(&self) -> usize {
        self.token_manager.total_count()
    }

    /// 发送非流式 API 请求，返回响应与最终成功的凭据 id（供监控按凭据统计）
    pub async fn call_api(&self, request: UpstreamRequest<'_>) -> anyhow::Result<UpstreamResponse> {
        self.call_api_with_retry(request, false).await
    }

    /// 发送流式 API 请求，返回响应与最终成功的凭据 id（供监控按凭据统计）
    pub async fn call_api_stream(
        &self,
        request: UpstreamRequest<'_>,
    ) -> anyhow::Result<UpstreamResponse> {
        self.call_api_with_retry(request, true).await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_mcp_with_retry(request_body).await
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let mut rpm_reserved: Option<u64> = None;

        for attempt in 0..max_retries {
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let rpm_mode = rpm_reserved
                .map(RpmChargeMode::Reuse)
                .unwrap_or(RpmChargeMode::Charge);
            let ctx = match self
                .token_manager
                .acquire_context(None, Capability::KiroOnly, false, rpm_mode)
                .await
            {
                Ok(c) => c,
                Err(e) if e.downcast_ref::<CredentialRpmExceeded>().is_some() => {
                    crate::metrics::inc_local_rpm_rejected();
                    return Err(local_rpm_limit_error(
                        e.downcast_ref::<CredentialRpmExceeded>().unwrap(),
                    ));
                }
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            if rpm_reserved.is_none() {
                rpm_reserved = Some(ctx.id);
            }

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config: &config,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json")
                .header("Connection", "close");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                crate::metrics::inc_request_success();
                return Ok(response);
            }

            // 在消费 body 前提取 Retry-After（429 限流时上游可能指示等待时长）
            let retry_after = Self::parse_retry_after(response.headers());

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    // 429 限流走专用指数退避（优先 Retry-After），其余瞬态错误走默认退避
                    let delay = if status.as_u16() == 429 {
                        Self::rate_limit_delay(attempt, retry_after)
                    } else {
                        Self::retry_delay(attempt)
                    };
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        record_request_failure_metrics(&last_error);
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request: UpstreamRequest<'_>,
        is_stream: bool,
    ) -> anyhow::Result<UpstreamResponse> {
        let UpstreamRequest {
            kiro_body,
            raw_body,
            capability: cap,
            passthrough_only,
            protocol,
            model_hint,
        } = request;
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let mut rpm_reserved: Option<u64> = None;
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 优先使用 handler 传入的 model，避免重复解析 JSON
        let model = model_hint
            .map(|s| s.to_string())
            .or_else(|| Self::extract_model_from_request(kiro_body));

        for attempt in 0..max_retries {
            let rpm_mode = rpm_reserved
                .map(RpmChargeMode::Reuse)
                .unwrap_or(RpmChargeMode::Charge);
            // 获取调用上下文（绑定 index、credentials、token）
            let ctx = match self
                .token_manager
                .acquire_context(model.as_deref(), cap, passthrough_only, rpm_mode)
                .await
            {
                Ok(c) => c,
                Err(e) if e.downcast_ref::<CredentialRpmExceeded>().is_some() => {
                    crate::metrics::inc_local_rpm_rejected();
                    return Err(local_rpm_limit_error(
                        e.downcast_ref::<CredentialRpmExceeded>().unwrap(),
                    ));
                }
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            if rpm_reserved.is_none() {
                rpm_reserved = Some(ctx.id);
            }

            let config = self.token_manager.config();

            // 透传凭据：原样转发原始入站字节到上游；额度耗尽为 403 + INSUFFICIENT_BALANCE
            let is_passthrough = ctx.credentials.is_passthrough();
            // Kiro 凭据的 endpoint（透传凭据为 None），用于失败响应的 body 关键字判定
            let mut kiro_endpoint: Option<Arc<dyn KiroEndpoint>> = None;

            let response = if is_passthrough {
                let request =
                    match self.build_passthrough_request(&ctx.credentials, protocol, raw_body) {
                        Ok(req) => req,
                        Err(e) => {
                            last_error = Some(e);
                            self.token_manager.report_failure(ctx.id);
                            continue;
                        }
                    };
                match request.send().await {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!(
                            "透传请求发送失败（尝试 {}/{}）: {}",
                            attempt + 1,
                            max_retries,
                            e
                        );
                        last_error = Some(e.into());
                        if attempt + 1 < max_retries {
                            sleep(Self::retry_delay(attempt)).await;
                        }
                        continue;
                    }
                }
            } else {
                let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config);

                let endpoint = match self.endpoint_for(&ctx.credentials) {
                    Ok(e) => e,
                    Err(e) => {
                        last_error = Some(e);
                        self.token_manager.report_failure(ctx.id);
                        continue;
                    }
                };
                kiro_endpoint = Some(endpoint.clone());

                let rctx = RequestContext {
                    credentials: &ctx.credentials,
                    token: &ctx.token,
                    machine_id: &machine_id,
                    config: &config,
                };

                let url = endpoint.api_url(&rctx);
                let body = endpoint.transform_api_body(kiro_body, &rctx);

                let base = self
                    .client_for(&ctx.credentials)?
                    .post(&url)
                    .body(body)
                    .header("content-type", "application/json")
                    .header("Connection", "close");
                let http_request = endpoint.decorate_api(base, &rctx);

                match http_request.send().await {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!(
                            "API 请求发送失败（尝试 {}/{}）: {}",
                            attempt + 1,
                            max_retries,
                            e
                        );
                        // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                        // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                        last_error = Some(e.into());
                        if attempt + 1 < max_retries {
                            sleep(Self::retry_delay(attempt)).await;
                        }
                        continue;
                    }
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                crate::metrics::inc_request_success();
                return Ok(UpstreamResponse {
                    response,
                    credential_id: ctx.id,
                    passthrough: is_passthrough,
                });
            }

            // 在消费 body 前提取 Retry-After（429 限流时上游可能指示等待时长）
            let retry_after = Self::parse_retry_after(response.headers());

            // 失败响应：读取 body 用于日志/错误信息
            let body = response.text().await.unwrap_or_default();

            // 透传凭据额度耗尽（403 + INSUFFICIENT_BALANCE）：等同 Kiro 的月度额度耗尽，
            // 禁用凭据并故障转移到池内其他同协议凭据。
            if is_passthrough
                && status.as_u16() == 403
                && body.contains(PASSTHROUGH_QUOTA_EXHAUSTED_MARKER)
            {
                tracing::warn!(
                    "透传凭据额度耗尽（禁用并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    return Err(anyhow::Error::new(UpstreamApiError {
                        status: 402,
                        message: format!(
                            "{} API 请求失败（所有凭据已用尽）: {} {}",
                            api_type, status, body
                        ),
                        retry_after: None,
                    }));
                }
                last_error = Some(anyhow::Error::new(UpstreamApiError {
                    status: 402,
                    message: format!("{} API 请求失败: {} {}", api_type, status, body),
                    retry_after: None,
                }));
                continue;
            }

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402
                && kiro_endpoint
                    .as_ref()
                    .is_some_and(|e| e.is_monthly_request_limit(&body))
            {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    return Err(anyhow::Error::new(UpstreamApiError {
                        status: 402,
                        message: format!(
                            "{} API 请求失败（所有凭据已用尽）: {} {}",
                            api_type, status, body
                        ),
                        retry_after: None,
                    }));
                }

                last_error = Some(anyhow::Error::new(UpstreamApiError {
                    status: 402,
                    message: format!("{} API 请求失败: {} {}", api_type, status, body),
                    retry_after: None,
                }));
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换凭据无意义
            if status.as_u16() == 400 {
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                // 透传凭据（kiro_endpoint 为 None）无 refresh token 机制，跳过
                if kiro_endpoint
                    .as_ref()
                    .is_some_and(|e| e.is_bearer_token_invalid(&body))
                    && !force_refreshed.contains(&ctx.id)
                {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
            // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::Error::new(UpstreamApiError {
                    status: status.as_u16(),
                    message: format!("{} API 请求失败: {} {}", api_type, status, body),
                    retry_after: if status.as_u16() == 429 {
                        retry_after
                    } else {
                        None
                    },
                }));
                if attempt + 1 < max_retries {
                    // 429 限流走专用指数退避（优先 Retry-After），其余瞬态错误走默认退避
                    let delay = if status.as_u16() == 429 {
                        Self::rate_limit_delay(attempt, retry_after)
                    } else {
                        Self::retry_delay(attempt)
                    };
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：当作可重试的瞬态错误处理（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        record_request_failure_metrics(&last_error);
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 解析 `Retry-After` 响应头
    ///
    /// 仅支持「整数秒」格式（429 限流场景最常见），HTTP-date 格式返回 None
    /// 由调用方回退到指数退避。
    fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
        let value = headers
            .get(reqwest::header::RETRY_AFTER)?
            .to_str()
            .ok()?
            .trim();
        let secs = value.parse::<u64>().ok()?;
        Some(Duration::from_secs(secs))
    }

    /// 429 限流专用退避策略
    ///
    /// 关键点：429 时不要立即重试，使用指数退避避免「重试风暴」。
    /// - 若上游返回 `Retry-After`，优先采用（并设上限避免请求长时间挂起）
    /// - 否则使用更激进的指数退避（1s → 2s → 4s … 上限 30s）+ 抖动
    ///   抖动用于打散并发请求的重试时刻，避免惊群（thundering herd）。
    fn rate_limit_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
        const CAP_MS: u64 = 30_000;

        if let Some(ra) = retry_after {
            let capped = ra.min(Duration::from_millis(CAP_MS));
            let jitter = Duration::from_millis(fastrand::u64(0..=250));
            return capped.saturating_add(jitter);
        }

        const BASE_MS: u64 = 1_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(CAP_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }
}

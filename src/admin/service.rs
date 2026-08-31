//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::anthropic::SharedApiKey;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, AppConfigResponse, BalanceResponse,
    CredentialStatusItem, CredentialsStatusResponse, DimBucketDto, LoadBalancingModeResponse,
    MetricsResponse, ModelStat, SetLoadBalancingModeRequest, TimeBucketDto, TimeseriesResponse,
    UpdateAppConfigRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 时间序列分布点 → DTO
fn dim_to_dto(d: crate::stats_db::DimPoint) -> DimBucketDto {
    DimBucketDto {
        key: d.key,
        requests: d.requests,
        input_tokens: d.input_tokens,
        output_tokens: d.output_tokens,
        cache_read_tokens: d.cache_read_tokens,
        cache_write_tokens: d.cache_write_tokens,
        credits: d.credits,
    }
}

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    /// 客户端 API Key 共享句柄（与 AppState 共享，用于热替换 apiKey）
    shared_api_key: SharedApiKey,
    /// Admin API Key 共享句柄（与 AdminState 认证中间件共享，用于热替换 adminApiKey）
    shared_admin_api_key: SharedApiKey,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
        shared_api_key: SharedApiKey,
        shared_admin_api_key: SharedApiKey,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            shared_api_key,
            shared_admin_api_key,
        }
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
                kind: entry.kind,
                base_url: entry.base_url,
                rpm: entry.rpm,
                cached_balance: self.cached_balance_fresh(entry.id),
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 查询监控时间序列（按小时/天聚合 + 按模型/凭据分布）
    ///
    /// `from`/`to` 为 Unix 秒，缺省为最近 24 小时。`bucket` 为 `hour`（默认）或 `day`。
    pub fn query_timeseries(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        bucket: Option<&str>,
    ) -> TimeseriesResponse {
        let now = Utc::now().timestamp();
        let to = to.unwrap_or(now);
        let from = from.unwrap_or(to - 24 * 3600);
        let bucket_enum = crate::stats_db::Bucket::parse(bucket.unwrap_or("hour"));
        let bucket_str = match bucket_enum {
            crate::stats_db::Bucket::Day => "day",
            crate::stats_db::Bucket::Hour => "hour",
        }
        .to_string();

        let data = crate::stats_db::global().query(from, to, bucket_enum);

        TimeseriesResponse {
            from,
            to,
            bucket: bucket_str,
            series: data
                .series
                .into_iter()
                .map(|p| TimeBucketDto {
                    bucket: p.bucket,
                    requests: p.requests,
                    input_tokens: p.input_tokens,
                    output_tokens: p.output_tokens,
                    cache_read_tokens: p.cache_read_tokens,
                    cache_write_tokens: p.cache_write_tokens,
                    credits: p.credits,
                })
                .collect(),
            by_model: data.by_model.into_iter().map(dim_to_dto).collect(),
            by_credential: data.by_credential.into_iter().map(dim_to_dto).collect(),
        }
    }

    /// 获取监控指标（进程级计数器快照 + 凭据池概览 + 各模型统计）
    pub fn get_metrics(&self) -> MetricsResponse {
        let m = crate::metrics::METRICS.snapshot();
        let snapshot = self.token_manager.snapshot();
        MetricsResponse {
            requests_success: m.requests_success,
            requests_error: m.requests_error,
            local_rpm_rejected: m.local_rpm_rejected,
            stream_decode_failures: m.stream_decode_failures,
            upstream_rate_limited: m.upstream_rate_limited,
            stream_interrupted: m.stream_interrupted,
            stream_restarted: m.stream_restarted,
            uptime_seconds: m.uptime_seconds,
            credentials_available: snapshot.available,
            credentials_total: snapshot.total,
            current_id: snapshot.current_id,
            models: self.build_model_stats(),
        }
    }

    /// 合并累计统计（次数/token）与实时 RPM，按归一化模型展示名聚合。
    ///
    /// 两个来源都以「客户端原始模型名」为键，这里统一归一为 displayId 后合并，
    /// 使 thinking 变体、别名归并到同一行。
    fn build_model_stats(&self) -> Vec<ModelStat> {
        use std::collections::HashMap;

        // 累计统计：原始名 → 各项，归一后累加
        #[derive(Default)]
        struct Agg {
            requests: u64,
            input_tokens: u64,
            output_tokens: u64,
            total_tokens: u64,
            credits: f64,
            today_requests: u64,
            today_input_tokens: u64,
            today_output_tokens: u64,
            today_total_tokens: u64,
            today_credits: f64,
            rpm: u32,
        }
        let mut merged: HashMap<String, Agg> = HashMap::new();

        for s in crate::model_stats::global().snapshot() {
            let key = crate::anthropic::canonical_model_display(&s.model);
            let e = merged.entry(key).or_default();
            e.requests += s.requests;
            e.input_tokens += s.input_tokens;
            e.output_tokens += s.output_tokens;
            e.total_tokens += s.total_tokens;
            e.credits += s.credits;
            e.today_requests += s.today_requests;
            e.today_input_tokens += s.today_input_tokens;
            e.today_output_tokens += s.today_output_tokens;
            e.today_total_tokens += s.today_total_tokens;
            e.today_credits += s.today_credits;
        }

        for (raw, count) in self.token_manager.model_rpm_snapshot() {
            let key = crate::anthropic::canonical_model_display(&raw);
            let e = merged.entry(key).or_default();
            e.rpm += count;
        }

        let mut list: Vec<ModelStat> = merged
            .into_iter()
            .map(|(model, a)| ModelStat {
                model,
                requests: a.requests,
                input_tokens: a.input_tokens,
                output_tokens: a.output_tokens,
                total_tokens: a.total_tokens,
                credits: a.credits,
                today_requests: a.today_requests,
                today_input_tokens: a.today_input_tokens,
                today_output_tokens: a.today_output_tokens,
                today_total_tokens: a.today_total_tokens,
                today_credits: a.today_credits,
                rpm: a.rpm,
            })
            .collect();
        // 按累计次数降序，其次按实时 RPM 降序，最后按名称稳定排序
        list.sort_by(|a, b| {
            b.requests
                .cmp(&a.requests)
                .then_with(|| b.rpm.cmp(&a.rpm))
                .then_with(|| a.model.cmp(&b.model))
        });
        list
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 读取本地缓存中未过期的余额（不触发上游请求）
    fn cached_balance_fresh(&self, id: u64) -> Option<BalanceResponse> {
        let cache = self.balance_cache.lock();
        let cached = cache.get(&id)?;
        let now = Utc::now().timestamp() as f64;
        if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
            Some(cached.data.clone())
        } else {
            None
        }
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 透传凭据走上游 /v1/usage（USD，balance 可为负）
        if self.token_manager.is_passthrough_credential(id) {
            return self.fetch_passthrough_balance(id).await;
        }

        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
        })
    }

    /// 获取透传凭据余额并映射到统一的 BalanceResponse
    ///
    /// 上游 /v1/usage 单位为 USD，只有钱包余额 `balance`（可为负），无硬性上限。
    /// 映射：`remaining` = balance；余额 <= 0 视为耗尽（usage_percentage = 100）。
    async fn fetch_passthrough_balance(
        &self,
        id: u64,
    ) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_passthrough_usage_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let remaining = usage.remaining_balance();
        // 钱包模式无硬上限：耗尽（<=0）按 100%，否则 0%（无从得知已用比例）
        let usage_percentage = if remaining <= 0.0 { 100.0 } else { 0.0 };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.plan_name.clone(),
            current_usage: 0.0,
            usage_limit: 0.0,
            remaining,
            usage_percentage,
            next_reset_at: None,
        })
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 构建凭据对象
        let email = req.email.clone();
        let mut new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
            kind: req.kind,
            base_url: req.base_url,
            upstream_api_key: req.upstream_api_key,
        };

        // 透传凭据无 Kiro 认证概念：清掉默认带上的 authMethod（前端默认 social）
        if new_cred.is_passthrough() {
            new_cred.auth_method = None;
        }

        // 透传凭据校验：必须同时有 baseUrl + upstreamApiKey
        if new_cred.is_passthrough()
            && (new_cred.base_url.as_deref().unwrap_or("").is_empty()
                || new_cred
                    .upstream_api_key
                    .as_deref()
                    .unwrap_or("")
                    .is_empty())
        {
            return Err(AdminServiceError::InvalidCredential(
                "透传凭据（kind=anthropic/openai）必须提供 baseUrl 和 upstreamApiKey".to_string(),
            ));
        }

        // 规范化认证方式（builder-id / iam -> idc），用于后续判定是否需要获取订阅等级
        new_cred.canonicalize_auth_method();
        let is_idc = new_cred.is_idc();

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤。
        // IdC 账号没有 FREE/PRO 订阅等级概念，getUsageLimits 接口对其不适用
        // （会返回 "Invalid profileArn"）；但 IdC 需要 profileArn 才能正常请求，
        // 因此改为主动获取并持久化 Profile ARN。
        // 透传凭据无 Kiro 订阅/profileArn 概念，跳过（getUsageLimits 对其不适用）。
        if self.token_manager.is_passthrough_credential(credential_id) {
            // 透传凭据无需获取 Kiro 订阅等级
        } else if is_idc {
            if let Err(e) = self
                .token_manager
                .ensure_profile_arn_for(credential_id)
                .await
            {
                tracing::warn!(
                    "添加 IdC 凭据后获取 Profile ARN 失败（不影响凭据添加）: {}",
                    e
                );
            }
        } else if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" && req.mode != "round-robin" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority'、'balanced' 或 'round-robin'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    // ============ 应用配置（页面可编辑子集） ============

    /// 获取可在页面编辑的应用配置当前值
    pub fn get_app_config(&self) -> AppConfigResponse {
        let config = self.token_manager.config();
        AppConfigResponse {
            api_key: self.shared_api_key.read().clone(),
            admin_api_key: self.shared_admin_api_key.read().clone(),
            credential_rpm: config.credential_rpm,
            credential_rpm_opus: config.credential_rpm_opus,
            credential_rpm_sonnet: config.credential_rpm_sonnet,
            credential_rpm_haiku: config.credential_rpm_haiku,
            credential_rpm_max_wait_ms: config.credential_rpm_max_wait_ms,
            kiro_version: config.kiro_version.clone(),
            machine_id: config.machine_id.clone(),
            system_version: config.system_version.clone(),
            node_version: config.node_version.clone(),
            streaming_sdk_version: config.streaming_sdk_version.clone(),
            models: config.effective_models(),
            default_model: config.default_model.clone(),
            model_aliases: config.model_aliases.clone(),
            chunked_write_policy: config.chunked_write_policy.clone(),
            codex_truncation_correction: config.codex_truncation_correction,
            proxy_url: config.proxy_url.clone(),
            proxy_username: config.proxy_username.clone(),
            proxy_password_set: config
                .proxy_password
                .as_ref()
                .is_some_and(|p| !p.is_empty()),
        }
    }

    /// 更新可编辑的应用配置子集，回写 config.json 并热生效
    ///
    /// 流程：校验 → 重新读盘（保留 host/port/adminApiKey 等未编辑字段）→ 改字段 →
    /// save() 回写 → 热应用（token_manager.replace_config + 模型注册表 + apiKey 句柄）。
    pub fn update_app_config(
        &self,
        req: UpdateAppConfigRequest,
    ) -> Result<AppConfigResponse, AdminServiceError> {
        use crate::model::config::Config;

        // ---- 校验 ----
        let api_key = req.api_key.trim();
        if api_key.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "apiKey 不能为空".to_string(),
            ));
        }
        // adminApiKey：提供时不能为空（缺省表示不修改）
        let admin_api_key: Option<String> = match &req.admin_api_key {
            Some(k) => {
                let trimmed = k.trim();
                if trimmed.is_empty() {
                    return Err(AdminServiceError::InvalidCredential(
                        "adminApiKey 不能为空".to_string(),
                    ));
                }
                Some(trimmed.to_string())
            }
            None => None,
        };
        if req.kiro_version.trim().is_empty()
            || req.system_version.trim().is_empty()
            || req.node_version.trim().is_empty()
            || req.streaming_sdk_version.trim().is_empty()
        {
            return Err(AdminServiceError::InvalidCredential(
                "kiroVersion / systemVersion / nodeVersion / streamingSdkVersion 均不能为空"
                    .to_string(),
            ));
        }
        if req.models.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "models 至少需要一个模型定义".to_string(),
            ));
        }
        for (i, m) in req.models.iter().enumerate() {
            if m.family.trim().is_empty()
                || m.kiro_id.trim().is_empty()
                || m.display_id.trim().is_empty()
                || m.display_name.trim().is_empty()
            {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "第 {} 个模型的 family / kiroId / displayId / displayName 均不能为空",
                    i + 1
                )));
            }
            if m.max_tokens <= 0 || m.context_window <= 0 {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "第 {} 个模型的 maxTokens / contextWindow 必须为正数",
                    i + 1
                )));
            }
        }
        for (from, to) in &req.model_aliases {
            if from.trim().is_empty() || to.trim().is_empty() {
                return Err(AdminServiceError::InvalidCredential(
                    "modelAliases 的键和值均不能为空".to_string(),
                ));
            }
        }
        if req
            .default_model
            .as_ref()
            .is_some_and(|s| s.trim().is_empty())
        {
            return Err(AdminServiceError::InvalidCredential(
                "defaultModel 不能为空字符串".to_string(),
            ));
        }
        // 分块写入策略：仅在启用时校验行数（关闭时数值无意义，允许任意残留值）
        if let Some(ref p) = req.chunked_write_policy
            && p.enabled
        {
            if p.chunk_lines == 0 {
                return Err(AdminServiceError::InvalidCredential(
                    "chunkedWritePolicy 的 chunkLines 必须为正数".to_string(),
                ));
            }
            if p.trigger_lines < p.chunk_lines {
                return Err(AdminServiceError::InvalidCredential(
                    "chunkedWritePolicy 的 triggerLines 不能小于 chunkLines".to_string(),
                ));
            }
        }

        let normalized_aliases: std::collections::HashMap<String, String> = req
            .model_aliases
            .iter()
            .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
            .collect();
        let default_model = req
            .default_model
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // ---- 读盘 → 改字段 → 回写 ----
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf());

        // 以磁盘上的最新配置为基底（保留 host/port/adminApiKey 等未编辑字段），
        // 路径未知时回退到当前内存配置。
        let mut new_config = match &config_path {
            Some(path) => Config::load(path).map_err(|e| {
                AdminServiceError::InternalError(format!("重新加载配置失败: {}", e))
            })?,
            None => (*self.token_manager.config()).clone(),
        };

        new_config.api_key = Some(api_key.to_string());
        if let Some(ref k) = admin_api_key {
            new_config.admin_api_key = Some(k.clone());
        }
        new_config.credential_rpm = req.credential_rpm;
        new_config.credential_rpm_opus = req.credential_rpm_opus;
        new_config.credential_rpm_sonnet = req.credential_rpm_sonnet;
        new_config.credential_rpm_haiku = req.credential_rpm_haiku;
        if let Some(max_wait_ms) = req.credential_rpm_max_wait_ms {
            new_config.credential_rpm_max_wait_ms = max_wait_ms;
        }
        if let Some(ref raw_machine_id) = req.machine_id {
            let trimmed = raw_machine_id.trim();
            new_config.machine_id = if trimmed.is_empty() {
                None
            } else {
                Some(
                    crate::kiro::machine_id::parse_configured_machine_id(trimmed).ok_or_else(
                        || {
                            AdminServiceError::InvalidCredential(
                                "machineId 格式无效（需 64 位十六进制或 UUID）".to_string(),
                            )
                        },
                    )?,
                )
            };
        }
        new_config.kiro_version = req.kiro_version.trim().to_string();
        new_config.system_version = req.system_version.trim().to_string();
        new_config.node_version = req.node_version.trim().to_string();
        new_config.streaming_sdk_version = req.streaming_sdk_version.trim().to_string();
        new_config.models = Some(req.models.clone());
        new_config.model_aliases = normalized_aliases.clone();
        new_config.default_model = default_model.clone();
        if let Some(ref policy) = req.chunked_write_policy {
            new_config.chunked_write_policy = policy.clone();
        }
        if let Some(enabled) = req.codex_truncation_correction {
            new_config.codex_truncation_correction = enabled;
        }
        // 全局代理：URL/用户名空串清除、缺省不改；密码空串清除、缺省保留原值。
        // 注意：provider 的 client 按代理缓存且构造时固定，改代理需重启进程才生效。
        if let Some(ref url) = req.proxy_url {
            let trimmed = url.trim();
            new_config.proxy_url = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Some(ref user) = req.proxy_username {
            let trimmed = user.trim();
            new_config.proxy_username = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Some(ref pass) = req.proxy_password {
            // 空串显式清除；非空则更新。字段缺失（None）时保留 new_config 原值。
            new_config.proxy_password = if pass.is_empty() {
                None
            } else {
                Some(pass.clone())
            };
        }

        if config_path.is_some() {
            new_config.save().map_err(|e| {
                AdminServiceError::InternalError(format!("回写配置文件失败: {}", e))
            })?;
        } else {
            tracing::warn!("配置文件路径未知，应用配置仅在当前进程生效");
        }

        // ---- 热应用 ----
        *self.shared_api_key.write() = api_key.to_string();
        if let Some(k) = admin_api_key {
            // 热替换 adminApiKey：认证中间件与本服务共享该句柄，立即生效
            *self.shared_admin_api_key.write() = k;
        }
        crate::anthropic::init_model_mapping(req.models, normalized_aliases, default_model);
        crate::anthropic::set_chunked_write_policy(new_config.chunked_write_policy.clone());
        crate::openai::set_codex_truncation_correction(new_config.codex_truncation_correction);
        self.token_manager.replace_config(new_config);

        tracing::info!("应用配置已更新并热生效");
        Ok(self.get_app_config())
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据")
        {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }
}

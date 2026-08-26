//! Admin API 类型定义

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::config::{ChunkedWritePolicy, ModelDef};

// ============ 凭据状态 ============

/// 所有凭据状态响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsStatusResponse {
    /// 凭据总数
    pub total: usize,
    /// 可用凭据数量（未禁用）
    pub available: usize,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 各凭据状态列表
    pub credentials: Vec<CredentialStatusItem>,
}

/// 单个凭据的状态信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatusItem {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级（数字越小优先级越高）
    pub priority: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 是否为当前活跃凭据
    pub is_current: bool,
    /// Token 过期时间（RFC3339 格式）
    pub expires_at: Option<String>,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 端点名称（决定该凭据走哪套 Kiro API，已回退到默认端点）
    pub endpoint: String,
    /// 凭据级 RPM 实时状态（各模型类别当前 60 秒窗口占用 + 生效上限）
    pub rpm: crate::kiro::token_manager::RpmStatus,
    /// 本地缓存的余额（仅未过期时带回，供前端刷新后免手动查询即可展示；无缓存则为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_balance: Option<BalanceResponse>,
}

// ============ 操作请求 ============

/// 启用/禁用凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledRequest {
    /// 是否禁用
    pub disabled: bool,
}

/// 修改优先级请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPriorityRequest {
    /// 新优先级值
    pub priority: u32,
}

/// 添加凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialRequest {
    /// 刷新令牌（OAuth 凭据必填，API Key 凭据不需要）
    pub refresh_token: Option<String>,

    /// 认证方式（可选，默认 social）
    #[serde(default = "default_auth_method")]
    pub auth_method: String,

    /// OIDC Client ID（IdC 认证需要）
    pub client_id: Option<String>,

    /// OIDC Client Secret（IdC 认证需要）
    pub client_secret: Option<String>,

    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    pub api_region: Option<String>,

    /// 凭据级 Machine ID（可选，64 位字符串）
    /// 未配置时回退到 config.json 的 machineId
    pub machine_id: Option<String>,

    /// 用户邮箱（可选，用于前端显示）
    pub email: Option<String>,

    /// 凭据级代理 URL（可选，特殊值 "direct" 表示不使用代理）
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    pub proxy_password: Option<String>,

    /// Kiro API Key（API Key 凭据必填，格式: ksk_xxxxxxxx）
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 端点名称（可选，未配置时使用 config.defaultEndpoint）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

fn default_auth_method() -> String {
    "social".to_string()
}

/// 添加凭据成功响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialResponse {
    pub success: bool,
    pub message: String,
    /// 新添加的凭据 ID
    pub credential_id: u64,
    /// 用户邮箱（如果获取成功）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

// ============ 余额查询 ============

/// 余额查询响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    /// 凭据 ID
    pub id: u64,
    /// 订阅类型
    pub subscription_title: Option<String>,
    /// 当前使用量
    pub current_usage: f64,
    /// 使用限额
    pub usage_limit: f64,
    /// 剩余额度
    pub remaining: f64,
    /// 使用百分比
    pub usage_percentage: f64,
    /// 下次重置时间（Unix 时间戳）
    pub next_reset_at: Option<f64>,
}

// ============ 负载均衡配置 ============

/// 负载均衡模式响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancingModeResponse {
    /// 当前模式（"priority"、"balanced" 或 "round-robin"）
    pub mode: String,
}

/// 设置负载均衡模式请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLoadBalancingModeRequest {
    /// 模式（"priority"、"balanced" 或 "round-robin"）
    pub mode: String,
}

// ============ 应用配置（页面可编辑子集） ============

/// 应用配置响应（页面可编辑字段的当前值）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfigResponse {
    /// 客户端 API Key
    pub api_key: String,
    /// 单凭据兜底 RPM
    pub credential_rpm: u32,
    /// Opus 专用 RPM
    pub credential_rpm_opus: Option<u32>,
    /// Sonnet 专用 RPM
    pub credential_rpm_sonnet: Option<u32>,
    /// Haiku 专用 RPM
    pub credential_rpm_haiku: Option<u32>,
    /// RPM 打满时最多等待毫秒数（0 = 立即 429）
    pub credential_rpm_max_wait_ms: u64,
    /// Kiro 客户端版本
    pub kiro_version: String,
    /// 全局 machineId（未配置凭据级时使用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// 系统版本指纹
    pub system_version: String,
    /// Node 版本
    pub node_version: String,
    /// CodeWhisperer Streaming SDK 版本（`@aws/codewhisperer-streaming-client`）
    pub streaming_sdk_version: String,
    /// 模型列表（生效值，缺省时为内置默认表）
    pub models: Vec<ModelDef>,
    /// OpenAI/Codex 未识别模型名的回退目标（displayId / kiroId）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// 客户端模型名 → 本服务模型名
    pub model_aliases: HashMap<String, String>,
    /// Write/Edit 分块写入策略
    pub chunked_write_policy: ChunkedWritePolicy,
    /// codex（/v1/responses）工具参数截断纠正开关
    pub codex_truncation_correction: bool,
}

/// 更新应用配置请求（全量替换可编辑子集）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAppConfigRequest {
    /// 客户端 API Key（不能为空）
    pub api_key: String,
    /// 单凭据兜底 RPM
    #[serde(default)]
    pub credential_rpm: u32,
    /// Opus 专用 RPM
    #[serde(default)]
    pub credential_rpm_opus: Option<u32>,
    /// Sonnet 专用 RPM
    #[serde(default)]
    pub credential_rpm_sonnet: Option<u32>,
    /// Haiku 专用 RPM
    #[serde(default)]
    pub credential_rpm_haiku: Option<u32>,
    /// RPM 打满时最多等待毫秒数（0 = 立即 429）；缺省时不更新（兼容旧 Admin UI）
    pub credential_rpm_max_wait_ms: Option<u64>,
    /// Kiro 客户端版本（不能为空）
    pub kiro_version: String,
    /// 全局 machineId（空字符串表示清除；字段缺失则不更新，兼容旧 UI）
    #[serde(default)]
    pub machine_id: Option<String>,
    /// 系统版本指纹（不能为空）
    pub system_version: String,
    /// Node 版本（不能为空）
    pub node_version: String,
    /// CodeWhisperer Streaming SDK 版本（不能为空）
    pub streaming_sdk_version: String,
    /// 模型列表（至少一个）
    pub models: Vec<ModelDef>,
    /// OpenAI/Codex 未识别模型名的回退目标
    #[serde(default)]
    pub default_model: Option<String>,
    /// 客户端模型名 → 本服务模型名
    #[serde(default)]
    pub model_aliases: HashMap<String, String>,
    /// Write/Edit 分块写入策略；缺省时不更新（兼容旧 Admin UI）
    #[serde(default)]
    pub chunked_write_policy: Option<ChunkedWritePolicy>,
    /// codex 工具参数截断纠正开关；缺省时不更新（兼容旧 Admin UI）
    #[serde(default)]
    pub codex_truncation_correction: Option<bool>,
}

// ============ 监控指标 ============

/// 单个模型的统计（累计调用/ token + 实时 RPM）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStat {
    /// 模型展示名
    pub model: String,
    /// 累计调用次数
    pub requests: u64,
    /// 累计输入 token
    pub input_tokens: u64,
    /// 累计输出 token
    pub output_tokens: u64,
    /// 累计总 token
    pub total_tokens: u64,
    /// 累计费用（credits）
    pub credits: f64,
    /// 今日调用次数
    pub today_requests: u64,
    /// 今日输入 token
    pub today_input_tokens: u64,
    /// 今日输出 token
    pub today_output_tokens: u64,
    /// 今日总 token
    pub today_total_tokens: u64,
    /// 今日费用（credits）
    pub today_credits: f64,
    /// 最近 60 秒调用数（实时 RPM，跨凭据聚合）
    pub rpm: u32,
}

/// 监控指标响应（进程级计数器快照 + 凭据池概览）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsResponse {
    /// 成功完成的上游 API 请求数
    pub requests_success: u64,
    /// 重试耗尽或不可恢复的上游 API 错误数
    pub requests_error: u64,
    /// 本地凭据 RPM 限流拒绝数（客户端 429）
    pub local_rpm_rejected: u64,
    /// 上游 event-stream 解码失败数
    pub stream_decode_failures: u64,
    /// 上游返回 429 且重试耗尽次数
    pub upstream_rate_limited: u64,
    /// 上游响应体传输中途断开次数
    pub stream_interrupted: u64,
    /// 断流后透明重连重放请求次数
    pub stream_restarted: u64,
    /// 进程运行时长（秒）
    pub uptime_seconds: u64,
    /// 当前可用（未禁用）凭据数
    pub credentials_available: usize,
    /// 凭据总数
    pub credentials_total: usize,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 各模型统计（按调用次数降序）
    pub models: Vec<ModelStat>,
}

// ============ 监控时间序列 ============

/// 时间序列查询参数（GET /api/admin/metrics/timeseries）
#[derive(Debug, Deserialize)]
pub struct TimeseriesQuery {
    /// 起始时间（Unix 秒，含）；缺省为 24 小时前
    pub from: Option<i64>,
    /// 结束时间（Unix 秒，不含）；缺省为当前时刻
    pub to: Option<i64>,
    /// 聚合粒度：`hour`（默认）或 `day`
    pub bucket: Option<String>,
}

/// 单个时间桶
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeBucketDto {
    /// 桶起始 Unix 秒
    pub bucket: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 计费 credits 消耗合计
    pub credits: f64,
}

/// 某维度（模型 / 凭据）区间聚合
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DimBucketDto {
    /// 模型展示名，或凭据 id 字符串
    pub key: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 计费 credits 消耗合计
    pub credits: f64,
}

/// 时间序列响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeseriesResponse {
    /// 实际生效的起止与粒度（回显，便于前端对齐）
    pub from: i64,
    pub to: i64,
    pub bucket: String,
    pub series: Vec<TimeBucketDto>,
    pub by_model: Vec<DimBucketDto>,
    pub by_credential: Vec<DimBucketDto>,
}

// ============ 通用响应 ============

/// 操作成功响应
#[derive(Debug, Serialize)]
pub struct SuccessResponse {
    pub success: bool,
    pub message: String,
}

impl SuccessResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }
}

/// 错误响应
#[derive(Debug, Serialize)]
pub struct AdminErrorResponse {
    pub error: AdminError,
}

#[derive(Debug, Serialize)]
pub struct AdminError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl AdminErrorResponse {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: AdminError {
                error_type: error_type.into(),
                message: message.into(),
            },
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }

    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid or missing admin API key")
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn api_error(message: impl Into<String>) -> Self {
        Self::new("api_error", message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
}

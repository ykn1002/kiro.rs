use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::NativeTls
    }
}

/// Write/Edit 工具的分块写入策略
///
/// 启用后会向 `Write`/`Edit` 工具的 description 追加「超长内容必须分块写入」的
/// 约束，并在系统提示词末尾追加「静默遵守分块限制」的策略说明。
///
/// 存在的原因是模型输出超长 tool_use 时会被截断，参数 JSON 不完整、整次调用作废
/// （上游表现为 `ContentLengthExceededException`）。分块把一次写入拆成多次工具
/// 往返以规避该上限。
///
/// # 与 Kiro IDE 官方实现对齐
///
/// 阈值单位与默认值取自 Kiro IDE 本体（`kiro.kiro-agent/dist/extension.js`）
/// 导出的 `WRITE_LIMIT` 常量，其值为字符串 `"50 lines"`，用于三处：
/// `fs_write` 的 description、输入超限错误、以及输出被截断后回灌给模型的重试
/// 指令。官方用**行数**而非字节/字符，且该常量是纯提示词文本、不参与任何程序
/// 校验——因此这里同样只做提示词注入，不在代理侧强制截断。
///
/// 官方以同一个值同时表示「触发阈值」与「每块上限」（内容超过 N 行就分块，
/// 每块不超过 N 行）。这里拆成两个字段：`triggerLines` 控制多大才需要分块，
/// `chunkLines` 控制每块多大。默认 150 / 50——即 150 行以内一次写完（省一次
/// 请求），超过则按 50 行一块，与官方每块上限一致。
///
/// 代价：Kiro 按请求次数计费，分块会**增加**配额消耗，故默认关闭。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkedWritePolicy {
    /// 是否启用
    pub enabled: bool,

    /// 分块触发阈值：内容超过该行数才要求分块
    ///
    /// 不超过时允许一次写完，避免小文件被无谓拆成多次请求。默认 150。
    /// 应 >= `chunk_lines`，否则等于每次都分块。
    pub trigger_lines: u32,

    /// 每块行数上限（含 Write 首块与后续 Edit 追加）
    ///
    /// 默认 50，与 Kiro 官方 `WRITE_LIMIT` 一致。截断纠正指令也用这个值。
    pub chunk_lines: u32,
}

/// 旧配置的字节字段折算回行数的系数
///
/// 取自本仓库平均行长（约 34.6 字节/行）取整。仅用于兼容曾短暂存在的
/// `triggerBytes` / `chunkBytes` 写法，折算后会打 warn 提示改用行数字段。
const LEGACY_BYTES_PER_LINE: u32 = 35;

impl Default for ChunkedWritePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_lines: default_trigger_lines(),
            chunk_lines: default_chunk_lines(),
        }
    }
}

/// 反序列化接受以下写法（后两种为兼容历史配置）：
/// 1. 推荐：`{"enabled": true, "triggerLines": 150, "chunkLines": 50}`
/// 2. 单字段 `writeLimitLines`（曾与官方 `WRITE_LIMIT` 单值对齐）
///    —— 同时作为阈值与块大小，等价于 `triggerLines == chunkLines`
/// 3. 旧的字节字段：`{"enabled": true, "triggerBytes": 5250, "chunkBytes": 1750}`
///    —— 按 [`LEGACY_BYTES_PER_LINE`] 折回行数并打 warn
/// 4. 旧的裸布尔：`true` —— 启用并使用默认值
impl<'de> Deserialize<'de> for ChunkedWritePolicy {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Obj {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            trigger_lines: Option<u32>,
            #[serde(default)]
            chunk_lines: Option<u32>,
            // 单值写法：同时作为阈值与块大小
            #[serde(default)]
            write_limit_lines: Option<u32>,
            // 字节写法，仅在对应行数字段缺失时生效
            #[serde(default)]
            trigger_bytes: Option<u32>,
            #[serde(default)]
            chunk_bytes: Option<u32>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Obj(Obj),
        }
        // 向上取整，避免小字节值折算成 0 行
        let to_lines = |b: u32| b.div_ceil(LEGACY_BYTES_PER_LINE).max(1);
        Ok(match Raw::deserialize(d)? {
            Raw::Bool(enabled) => Self {
                enabled,
                ..Default::default()
            },
            Raw::Obj(o) => {
                if o.trigger_bytes.is_some() || o.chunk_bytes.is_some() {
                    tracing::warn!(
                        "chunkedWritePolicy 的 triggerBytes / chunkBytes 已废弃，\
                         请改用 triggerLines / chunkLines"
                    );
                }
                let chunk_lines = o
                    .chunk_lines
                    .or(o.write_limit_lines)
                    .or(o.chunk_bytes.map(to_lines))
                    .unwrap_or_else(default_chunk_lines);
                let trigger_lines = o
                    .trigger_lines
                    .or(o.write_limit_lines)
                    .or(o.trigger_bytes.map(to_lines))
                    .unwrap_or_else(default_trigger_lines);
                Self {
                    enabled: o.enabled,
                    // 阈值小于块大小没有意义，抬到块大小
                    trigger_lines: trigger_lines.max(chunk_lines),
                    chunk_lines,
                }
            }
        })
    }
}

/// 模型定义
///
/// 同时驱动三处逻辑：
/// 1. `map_model`：用 `family` + `version` 模糊匹配 Anthropic 模型名 → `kiro_id`
/// 2. `get_context_window_size`：命中后返回 `context_window`
/// 3. `GET /v1/models`：用 `display_id` / `display_name` / `created` / `max_tokens` 生成展示列表
///    （thinking 变体由代码自动派生，无需在配置中重复）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDef {
    /// 模型族，模糊匹配的第一段（小写匹配）："sonnet" / "opus" / "haiku"
    pub family: String,

    /// 版本号，如 "4.6"。匹配时同时尝试 "4-6" 和 "4.6" 两种写法。
    /// 为 `None` 时（如 haiku）仅靠 `family` 命中。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// 映射后的 Kiro 模型 ID，如 "claude-sonnet-4.6"
    pub kiro_id: String,

    /// `/v1/models` 展示用 ID，如 "claude-sonnet-4-6"
    pub display_id: String,

    /// `/v1/models` 展示名，如 "Claude Sonnet 4.6"
    pub display_name: String,

    /// `/v1/models` 的创建时间戳（Unix 秒）
    pub created: i64,

    /// `/v1/models` 的 max_tokens
    pub max_tokens: i32,

    /// 上下文窗口大小（200_000 / 1_000_000）
    pub context_window: i32,
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    /// `@aws/codewhisperer-streaming-client` 版本，用于主 API / MCP 的 aws-sdk-js User-Agent
    #[serde(default = "default_streaming_sdk_version")]
    pub streaming_sdk_version: String,

    /// `@aws-sdk/client-sso-oidc` 版本，用于 IdC Token 刷新 User-Agent；未配置时默认 `3.980.0`
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_oidc_sdk_version: Option<String>,

    /// `@amzn/codewhisperer-runtime` 版本，用于额度查询等 runtime API User-Agent；未配置时默认 `1.0.0`
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_sdk_version: Option<String>,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 负载均衡模式（"priority"、"balanced" 或 "round-robin"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 单凭据目标 RPM（每分钟请求数），用于凭据级节流/分流
    ///
    /// 当某个凭据在最近 60 秒内的请求数达到该值时，会在凭据选择时被跳过，
    /// 请求会被分流到其他可用凭据。`0` 或未配置表示不限制（使用内置默认策略）。
    ///
    /// 作为未匹配到 Opus/Sonnet 专用限制时的兜底值。
    #[serde(default)]
    pub credential_rpm: u32,

    /// 单凭据 Opus 模型专用 RPM，未配置时回退到 `credential_rpm`
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_rpm_opus: Option<u32>,

    /// 单凭据 Sonnet 模型专用 RPM，未配置时回退到 `credential_rpm`
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_rpm_sonnet: Option<u32>,

    /// 单凭据 Haiku 模型专用 RPM，未配置时回退到 `credential_rpm`
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_rpm_haiku: Option<u32>,

    /// 当所有可用凭据都达到 RPM 上限时，最多等待多少毫秒再放行（而非立即突发）。
    ///
    /// `0`（默认）表示不等待，RPM 打满后立即返回 429。
    /// 大于 0 时会在返回 429 前先等待至多该毫秒数，以便槽位滑出 60 秒窗口（平滑突发）。
    #[serde(default)]
    pub credential_rpm_max_wait_ms: u64,

    /// 是否在向客户端透传 429 时附带 `Retry-After` 响应头（默认 false）
    ///
    /// 开启后会把上游或本地 RPM 计算的等待秒数写入响应头；部分客户端会据此长时间退避。
    #[serde(default)]
    pub passthrough_retry_after: bool,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// Write/Edit 工具的分块写入策略（默认关闭）
    ///
    /// 见 [`ChunkedWritePolicy`]。为兼容旧配置，也接受裸布尔值
    /// （`"chunkedWritePolicy": true` 等价于启用并使用默认行数）。
    #[serde(default)]
    pub chunked_write_policy: ChunkedWritePolicy,

    /// codex（`/v1/responses`）端点工具参数被截断时是否回灌分块纠正文本（默认开）
    ///
    /// code mode 下模型写文件走标准 JSON tool_call，参数被 `max_tokens` 截断时
    /// （收到开始却无 `stop`），残缺调用会被静默丢弃、客户端一无所知。开启后追加
    /// 一段与 `/cc` 一致的纠正文本，提示模型分块写。行数取 `chunkedWritePolicy.chunk_lines`。
    ///
    /// 注意：挂空 item 的封口修复无条件生效，此开关只控制纠正文本的追加。
    #[serde(default = "default_codex_truncation_correction")]
    pub codex_truncation_correction: bool,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 模型列表
    ///
    /// 为 `None`/缺失时回退到内置默认表 `default_models()`（见 `effective_models`）。
    /// 完全向后兼容：老配置无需改动。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelDef>>,

    /// OpenAI/Codex 等客户端模型名 → 本服务模型名（displayId / kiroId / 别名）的显式映射。
    ///
    /// 键比较时不区分大小写。例如 Codex 发送 `gpt-5.5` 时可映射到 `claude-opus-4-6`。
    #[serde(default)]
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub model_aliases: HashMap<String, String>,

    /// 未命中任何模型规则时的回退模型名（displayId / kiroId / 别名）。
    ///
    /// 适用于 Codex 等固定发送 `gpt-5.x` 但后端实际走 Claude/Kiro 的场景。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "0.11.107".to_string()
}

fn default_system_version() -> String {
    const SYSTEM_VERSIONS: &[&str] = &["darwin#24.6.0", "win32#10.0.22631"];
    SYSTEM_VERSIONS[fastrand::usize(..SYSTEM_VERSIONS.len())].to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_streaming_sdk_version() -> String {
    "1.0.39".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::NativeTls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_codex_truncation_correction() -> bool {
    true
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

/// 分块写入的默认行数上限
///
/// 取自 Kiro IDE 本体导出的 `WRITE_LIMIT` 常量（值为 `"50 lines"`）。
fn default_chunk_lines() -> u32 {
    50
}

/// 分块触发阈值默认值
///
/// 官方对 `fs_write` 用同一个 50 既作阈值又作块大小；这里放宽到 150，
/// 让中小文件一次写完（按请求计费，少一次拆分就少一次扣费），只有确实
/// 很大的内容才分块。
fn default_trigger_lines() -> u32 {
    150
}

/// 内置默认模型表
///
/// 顺序即匹配优先级（与原 `map_model` 的 if-else 顺序一致）。
/// `get_models` / `map_model` / `get_context_window_size` 在配置未提供 `models` 时回退到此表。
pub fn default_models() -> Vec<ModelDef> {
    vec![
        ModelDef {
            family: "opus".to_string(),
            version: Some("4.8".to_string()),
            kiro_id: "claude-opus-4.8".to_string(),
            display_id: "claude-opus-4-8".to_string(),
            display_name: "Claude Opus 4.8".to_string(),
            created: 1779897600,
            max_tokens: 128_000,
            context_window: 1_000_000,
        },
        ModelDef {
            family: "opus".to_string(),
            version: Some("4.7".to_string()),
            kiro_id: "claude-opus-4.7".to_string(),
            display_id: "claude-opus-4-7".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            created: 1776276000,
            max_tokens: 64000,
            context_window: 1_000_000,
        },
        ModelDef {
            family: "opus".to_string(),
            version: Some("4.6".to_string()),
            kiro_id: "claude-opus-4.6".to_string(),
            display_id: "claude-opus-4-6".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            created: 1770163200,
            max_tokens: 64000,
            context_window: 1_000_000,
        },
        ModelDef {
            family: "opus".to_string(),
            version: Some("4.5".to_string()),
            kiro_id: "claude-opus-4.5".to_string(),
            display_id: "claude-opus-4-5-20251101".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            created: 1763942400,
            max_tokens: 64000,
            context_window: 200_000,
        },
        ModelDef {
            family: "sonnet".to_string(),
            version: Some("4.6".to_string()),
            kiro_id: "claude-sonnet-4.6".to_string(),
            display_id: "claude-sonnet-4-6".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            created: 1784249340,
            max_tokens: 64000,
            context_window: 1_000_000,
        },
        ModelDef {
            family: "sonnet".to_string(),
            version: Some("4.5".to_string()),
            kiro_id: "claude-sonnet-4.5".to_string(),
            display_id: "claude-sonnet-4-5-20250929".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            created: 1759104000,
            max_tokens: 64000,
            context_window: 200_000,
        },
        ModelDef {
            family: "haiku".to_string(),
            version: None,
            kiro_id: "claude-haiku-4.5".to_string(),
            display_id: "claude-haiku-4-5-20251001".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            created: 1760486400,
            max_tokens: 64000,
            context_window: 200_000,
        },
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            streaming_sdk_version: default_streaming_sdk_version(),
            sso_oidc_sdk_version: None,
            runtime_sdk_version: None,
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            load_balancing_mode: default_load_balancing_mode(),
            credential_rpm: 0,
            credential_rpm_opus: None,
            credential_rpm_sonnet: None,
            credential_rpm_haiku: None,
            credential_rpm_max_wait_ms: 0,
            passthrough_retry_after: false,
            extract_thinking: default_extract_thinking(),
            chunked_write_policy: ChunkedWritePolicy::default(),
            codex_truncation_correction: default_codex_truncation_correction(),
            default_endpoint: default_endpoint(),
            endpoints: HashMap::new(),
            models: None,
            model_aliases: HashMap::new(),
            default_model: None,
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// IdC Token 刷新用的 `@aws-sdk/client-sso-oidc` 版本
    pub fn effective_sso_oidc_sdk_version(&self) -> &str {
        self.sso_oidc_sdk_version
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("3.980.0")
    }

    /// 额度查询等 runtime API 用的 `@amzn/codewhisperer-runtime` 版本
    pub fn effective_runtime_sdk_version(&self) -> &str {
        self.runtime_sdk_version
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("1.0.0")
    }

    /// 获取有效的模型表
    /// 优先使用配置的 `models`，未配置时回退到内置默认表
    pub fn effective_models(&self) -> Vec<ModelDef> {
        self.models.clone().unwrap_or_else(default_models)
    }

    /// 模型别名表（键统一为小写）
    pub fn effective_model_aliases(&self) -> HashMap<String, String> {
        self.model_aliases
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.clone()))
            .collect()
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());
        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// chunkedWritePolicy 缺省时关闭，默认 150 触发 / 50 每块
    #[test]
    fn test_chunked_write_policy_default_disabled() {
        let cfg = Config::default();
        assert!(!cfg.chunked_write_policy.enabled);
        assert_eq!(cfg.chunked_write_policy.trigger_lines, 150);
        assert_eq!(cfg.chunked_write_policy.chunk_lines, 50);
    }

    /// 推荐写法应逐字段生效
    #[test]
    fn test_chunked_write_policy_object_form() {
        let p: ChunkedWritePolicy =
            serde_json::from_str(r#"{"enabled": true, "triggerLines": 300, "chunkLines": 120}"#)
                .unwrap();
        assert!(p.enabled);
        assert_eq!(p.trigger_lines, 300);
        assert_eq!(p.chunk_lines, 120);
    }

    /// 兼容旧配置的裸布尔写法：启用并回退默认值
    #[test]
    fn test_chunked_write_policy_bool_form_compat() {
        let p: ChunkedWritePolicy = serde_json::from_str("true").unwrap();
        assert!(p.enabled);
        assert_eq!(p.trigger_lines, 150);
        assert_eq!(p.chunk_lines, 50);

        let p: ChunkedWritePolicy = serde_json::from_str("false").unwrap();
        assert!(!p.enabled);
    }

    /// 兼容单值 writeLimitLines：同时作为阈值与块大小
    #[test]
    fn test_chunked_write_policy_single_value_compat() {
        let p: ChunkedWritePolicy =
            serde_json::from_str(r#"{"enabled": true, "writeLimitLines": 50}"#).unwrap();
        assert_eq!(p.trigger_lines, 50);
        assert_eq!(p.chunk_lines, 50);
    }

    /// 显式的双字段优先于单值写法
    #[test]
    fn test_explicit_fields_take_precedence_over_single_value() {
        let p: ChunkedWritePolicy = serde_json::from_str(
            r#"{"enabled": true, "writeLimitLines": 50,
                 "triggerLines": 150, "chunkLines": 60}"#,
        )
        .unwrap();
        assert_eq!(p.trigger_lines, 150);
        assert_eq!(p.chunk_lines, 60);
    }

    /// 兼容旧的字节字段：按 35 字节/行折回行数
    #[test]
    fn test_chunked_write_policy_legacy_bytes_folded_to_lines() {
        let p: ChunkedWritePolicy =
            serde_json::from_str(r#"{"enabled": true, "triggerBytes": 5250, "chunkBytes": 1750}"#)
                .unwrap();
        assert_eq!(p.trigger_lines, 150);
        assert_eq!(p.chunk_lines, 50);

        // 极小字节值不应折算成 0 行
        let p: ChunkedWritePolicy =
            serde_json::from_str(r#"{"enabled": true, "chunkBytes": 10}"#).unwrap();
        assert_eq!(p.chunk_lines, 1);
    }

    /// 阈值小于块大小时抬到块大小（否则等于每次都分块）
    #[test]
    fn test_trigger_lines_raised_to_chunk_lines() {
        let p: ChunkedWritePolicy =
            serde_json::from_str(r#"{"enabled": true, "triggerLines": 10, "chunkLines": 50}"#)
                .unwrap();
        assert_eq!(p.trigger_lines, 50);
        assert_eq!(p.chunk_lines, 50);
    }

    /// 对象写法省略行数字段时回退默认值
    #[test]
    fn test_chunked_write_policy_partial_object() {
        let p: ChunkedWritePolicy = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert!(p.enabled);
        assert_eq!(p.trigger_lines, 150);
        assert_eq!(p.chunk_lines, 50);
    }

    /// 缺省 models 时回退内置默认表
    #[test]
    fn test_effective_models_defaults_when_absent() {
        let cfg = Config::default();
        assert!(cfg.models.is_none());
        let models = cfg.effective_models();
        assert_eq!(models.len(), default_models().len());
        assert!(models.iter().any(|m| m.kiro_id == "claude-opus-4.8"));
    }

    /// 解析自定义 models（同步守护 README 完整配置示例的字段结构）
    #[test]
    fn test_parse_custom_models() {
        let json = r#"{
            "apiKey": "k",
            "models": [
                { "family": "opus", "version": "4.8", "kiroId": "claude-opus-4.8", "displayId": "claude-opus-4-8", "displayName": "Claude Opus 4.8", "created": 1779897600, "maxTokens": 128000, "contextWindow": 1000000 },
                { "family": "haiku", "kiroId": "claude-haiku-4.5", "displayId": "claude-haiku-4-5-20251001", "displayName": "Claude Haiku 4.5", "created": 1760486400, "maxTokens": 64000, "contextWindow": 200000 }
            ]
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("自定义 models 应能解析");
        let models = cfg.effective_models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].kiro_id, "claude-opus-4.8");
        assert_eq!(models[0].context_window, 1_000_000);
        assert!(models[1].version.is_none(), "haiku 省略 version 应为 None");
    }

    /// config.example.json 是桌面版首启模板，展示的是「当前推荐」模型组合；
    /// default_models() 是未配置 models 时的请求兜底表，需要长期保留旧模型 ID
    /// 以免旧客户端请求（如 claude-opus-4-7）失效。两者收录的模型集合可以不同，
    /// 这里只校验两边同时出现的模型（按 kiro_id 匹配）字段没有互相冲突，
    /// 避免样例和默认表各自维护出两份不一致的同名模型定义。
    #[test]
    fn test_example_config_models_consistent_with_defaults() {
        let json = include_str!("../../config.example.json");
        let cfg: Config = serde_json::from_str(json).expect("config.example.json 应能解析");
        let example = cfg.models.expect("样例应包含 models");
        let defaults = default_models();
        for d in defaults.iter() {
            let Some(e) = example.iter().find(|e| e.kiro_id == d.kiro_id) else {
                continue;
            };
            assert_eq!(e.family, d.family, "family 不一致: {}", d.kiro_id);
            assert_eq!(e.version, d.version, "version 不一致: {}", d.kiro_id);
            assert_eq!(e.display_id, d.display_id, "display_id 不一致: {}", d.kiro_id);
            assert_eq!(
                e.display_name, d.display_name,
                "display_name 不一致: {}",
                d.kiro_id
            );
            assert_eq!(e.created, d.created, "created 不一致: {}", d.kiro_id);
            assert_eq!(e.max_tokens, d.max_tokens, "max_tokens 不一致: {}", d.kiro_id);
            assert_eq!(
                e.context_window, d.context_window,
                "context_window 不一致: {}",
                d.kiro_id
            );
        }
    }
}

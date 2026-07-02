//! Anthropic API 兼容服务模块
//!
//! 提供与 Anthropic Claude API 兼容的 HTTP 服务端点。
//!
//! # 支持的端点
//!
//! ## 标准端点 (/v1)
//! - `GET /v1/models` - 获取可用模型列表
//! - `POST /v1/messages` - 创建消息（对话）
//! - `POST /v1/messages/count_tokens` - 计算 token 数量
//!
//! ## Claude Code 兼容端点 (/cc/v1)
//! - `GET /cc/v1/models` - 获取可用模型列表（与 /v1 相同）
//! - `POST /cc/v1/messages` - 创建消息（与 /v1 相同：等待 contextUsageEvent 后发送 message_start，之后实时流式输出）
//! - `POST /cc/v1/messages/count_tokens` - 计算 token 数量（与 /v1 相同）
//!
//! # 使用示例
//! ```rust,ignore
//! use kiro_rs::anthropic;
//!
//! let app = anthropic::create_router("your-api-key");
//! let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
//! axum::serve(listener, app).await?;
//! ```

mod converter;
mod handlers;
mod middleware;
mod router;
mod stream;
pub mod types;

pub(crate) use stream::{compute_thinking_signature, extract_thinking_from_complete_text};
mod websearch;

pub use converter::{init_model_mapping, init_model_registry, set_model_registry};
pub(crate) use converter::{ConversionError, convert_request, convert_responses_request, conversion_error_parts, get_context_window_size, map_model, metadata_from_openai_extra, normalize_tool_schema};
pub use middleware::SharedApiKey;
pub(crate) use middleware::AppState;
pub use router::create_router_with_provider;

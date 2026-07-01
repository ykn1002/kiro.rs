//! OpenAI API 兼容服务模块
//!
//! 提供 OpenAI 兼容端点，供 Codex CLI 等客户端使用。
//!
//! # 支持的端点
//!
//! - `POST /v1/chat/completions` - Chat Completions（转换模式）
//! - `POST /v1/responses` - Responses API（原生模式，Codex 默认）
//!
//! # Codex 配置示例
//!
//! 在 `~/.codex/config.toml` 中添加：
//!
//! ```toml
//! model_provider = "kiro"
//! model = "claude-sonnet-4-6"
//!
//! [model_providers.kiro]
//! name = "Kiro RS"
//! base_url = "http://127.0.0.1:8080/v1"
//! env_key = "KIRO_RS_API_KEY"
//! wire_api = "responses"
//! ```
//!
//! 然后设置环境变量：`export KIRO_RS_API_KEY=<config.json 中的 apiKey>`

mod converter;
mod handlers;
mod responses_converter;
mod responses_handlers;
mod responses_stream;
mod responses_types;
mod stream;
pub mod types;
mod utf8;

pub use handlers::chat_completions;
pub use responses_handlers::create_response;

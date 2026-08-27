//! kiro-rs：Anthropic Claude API 兼容代理。
//!
//! 本 crate 同时暴露为库（供桌面版 Tauri 壳等外部调用）与二进制。
//! 服务启动逻辑集中在 [`app`] 模块，`main.rs` 只做 CLI 解析后转发调用。

pub mod admin;
pub mod admin_ui;
pub mod anthropic;
pub mod app;
pub mod common;
pub mod http_client;
pub mod kiro;
pub mod metrics;
pub mod model;
pub mod model_stats;
pub mod openai;
pub mod stats_db;
pub mod token;

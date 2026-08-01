//! 元数据事件
//!
//! 处理 metadataEvent 类型的事件

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 精确 token 用量
///
/// 对应上游 `TokenUsage` 结构。相比 `contextUsageEvent` 只给百分比，
/// 这里是上游直接下发的精确计数，无需按窗口大小反推。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// 未命中缓存的输入 token 数
    #[serde(default)]
    pub uncached_input_tokens: Option<i64>,
    /// 输出 token 数
    #[serde(default)]
    pub output_tokens: Option<i64>,
    /// 输入 + 输出总计
    #[serde(default)]
    pub total_tokens: Option<i64>,
    /// 命中缓存读取的输入 token 数
    #[serde(default)]
    pub cache_read_input_tokens: Option<i64>,
    /// 写入缓存的输入 token 数
    #[serde(default)]
    pub cache_write_input_tokens: Option<i64>,
}

impl TokenUsage {
    /// 计算 Anthropic 语义的 `input_tokens`
    ///
    /// Anthropic 的 `input_tokens` 只统计未命中缓存的部分，缓存读写分别由
    /// `cache_read_input_tokens` / `cache_creation_input_tokens` 单独上报。
    /// 上游若未给 `uncached_input_tokens`，退化为用 `total_tokens` 减去
    /// 输出与缓存部分。
    pub fn anthropic_input_tokens(&self) -> Option<i32> {
        if let Some(uncached) = self.uncached_input_tokens {
            return Some(uncached.max(0) as i32);
        }

        let total = self.total_tokens?;
        let output = self.output_tokens.unwrap_or(0);
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_write = self.cache_write_input_tokens.unwrap_or(0);
        Some((total - output - cache_read - cache_write).max(0) as i32)
    }

    /// 输出 token 数（i32 截断，供 SSE usage 字段使用）
    pub fn anthropic_output_tokens(&self) -> Option<i32> {
        self.output_tokens.map(|t| t.max(0) as i32)
    }

    /// 是否含任何可用的 token 计数
    pub fn has_counts(&self) -> bool {
        self.uncached_input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.total_tokens.is_some()
    }
}

/// 元数据事件
///
/// 负载为 `{"tokenUsage": {...}}`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// 精确 token 用量
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

/// 非法状态事件
///
/// 上游主动上报会话状态非法（schema 中被标记为错误类型）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvalidStateEvent {
    /// 原因代码
    #[serde(default)]
    pub reason: String,
    /// 可读消息
    #[serde(default)]
    pub message: String,
}

impl EventPayload for InvalidStateEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl std::fmt::Display for InvalidStateEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.reason, self.message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uncached_input_tokens_preferred() {
        let usage: TokenUsage = serde_json::from_str(
            r#"{"uncachedInputTokens":100,"outputTokens":50,"totalTokens":900,
                "cacheReadInputTokens":750,"cacheWriteInputTokens":0}"#,
        )
        .unwrap();
        // 直接用 uncachedInputTokens，不做减法
        assert_eq!(usage.anthropic_input_tokens(), Some(100));
        assert_eq!(usage.anthropic_output_tokens(), Some(50));
        assert!(usage.has_counts());
    }

    #[test]
    fn test_input_tokens_derived_from_total() {
        // 缺 uncachedInputTokens 时用 total 减去输出与缓存部分
        let usage: TokenUsage = serde_json::from_str(
            r#"{"outputTokens":50,"totalTokens":900,"cacheReadInputTokens":700,
                "cacheWriteInputTokens":50}"#,
        )
        .unwrap();
        assert_eq!(usage.anthropic_input_tokens(), Some(100));
    }

    #[test]
    fn test_derived_input_tokens_never_negative() {
        let usage: TokenUsage =
            serde_json::from_str(r#"{"outputTokens":500,"totalTokens":100}"#).unwrap();
        assert_eq!(usage.anthropic_input_tokens(), Some(0));
    }

    #[test]
    fn test_empty_usage_has_no_counts() {
        let usage: TokenUsage = serde_json::from_str("{}").unwrap();
        assert!(!usage.has_counts());
        assert_eq!(usage.anthropic_input_tokens(), None);
        assert_eq!(usage.anthropic_output_tokens(), None);
    }

    #[test]
    fn test_metadata_event_unknown_fields_ignored() {
        // 上游新增字段不应导致解析失败
        let event: MetadataEvent = serde_json::from_str(
            r#"{"tokenUsage":{"outputTokens":7,"normalizedTokenUsage":{"x":1}},"somethingNew":true}"#,
        )
        .unwrap();
        let usage = event.token_usage.expect("应解析出 tokenUsage");
        assert_eq!(usage.anthropic_output_tokens(), Some(7));
    }

    #[test]
    fn test_invalid_state_event_display() {
        let event: InvalidStateEvent =
            serde_json::from_str(r#"{"reason":"INVALID_TOOL_RESULT","message":"missing id"}"#)
                .unwrap();
        assert_eq!(event.to_string(), "INVALID_TOOL_RESULT: missing id");

        let bare: InvalidStateEvent = serde_json::from_str(r#"{"reason":"OTHER"}"#).unwrap();
        assert_eq!(bare.to_string(), "OTHER");
    }
}

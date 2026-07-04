//! Kiro 请求类型定义
//!
//! 定义 Kiro API 的主请求结构

use serde::{Deserialize, Serialize};

use super::conversation::ConversationState;

/// Kiro API 请求
///
/// 用于构建发送给 Kiro API 的请求
///
/// # 示例
///
/// ```rust
/// use kiro_rs::kiro::model::requests::{
///     KiroRequest, ConversationState, CurrentMessage, UserInputMessage, Tool
/// };
///
/// // 创建简单请求
/// let state = ConversationState::new("conv-123")
///     .with_agent_task_type("vibe")
///     .with_current_message(CurrentMessage::new(
///         UserInputMessage::new("Hello", "claude-3-5-sonnet")
///     ));
///
/// let request = KiroRequest::new(state);
/// let json = request.to_json().unwrap();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroRequest {
    /// 对话状态
    pub conversation_state: ConversationState,
    /// Profile ARN（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    /// 与 HTTP 头 `x-amzn-kiro-agent-mode` 一致
    #[serde(default = "default_agent_mode")]
    pub agent_mode: String,
}

fn default_agent_mode() -> String {
    "vibe".to_string()
}

impl KiroRequest {
    /// 由转换后的 `ConversationState` 构建上游请求（`profileArn` 由 endpoint 注入）
    pub fn from_conversation_state(conversation_state: ConversationState) -> Self {
        Self {
            conversation_state,
            profile_arn: None,
            agent_mode: default_agent_mode(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{ConversationState, KiroRequest};
    #[test]
    fn test_kiro_request_deserialize() {
        let json = r#"{
            "conversationState": {
                "conversationId": "conv-456",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "Test message",
                        "modelId": "claude-3-5-sonnet",
                        "userInputMessageContext": {}
                    }
                }
            }
        }"#;

        let request: KiroRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.conversation_state.conversation_id, "conv-456");
        assert_eq!(
            request
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "Test message"
        );
    }

    #[test]
    fn test_kiro_request_serializes_agent_mode() {
        let request = KiroRequest::from_conversation_state(ConversationState::new("conv-1"));
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"agentMode\":\"vibe\""));
    }
}

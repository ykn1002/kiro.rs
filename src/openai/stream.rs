//! OpenAI 流式响应处理：Kiro 事件 → Chat Completions SSE chunks

use std::collections::HashMap;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::anthropic::get_context_window_size;
use crate::kiro::model::events::Event;

use super::utf8::find_char_boundary;

/// OpenAI SSE chunk 封装
#[derive(Debug, Clone)]
pub struct OpenAiChunk {
    pub data: serde_json::Value,
}

impl OpenAiChunk {
    pub fn to_sse_string(&self) -> String {
        format!("data: {}\n\n", self.data)
    }
}

struct ToolCallState {
    index: u32,
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

/// OpenAI 流式响应上下文
pub struct OpenAiStreamContext {
    completion_id: String,
    model: String,
    created: i64,
    tool_name_map: HashMap<String, String>,
    thinking_enabled: bool,
    /// tool_use_id → 状态
    tool_calls: HashMap<String, ToolCallState>,
    next_tool_index: u32,
    has_tool_use: bool,
    finish_reason: Option<String>,
    sent_role: bool,
    prompt_tokens: i32,
    completion_tokens: i32,
    text_buffer: String,
    in_thinking: bool,
    thinking_extracted: bool,
    pub stream_failed: bool,
}

impl OpenAiStreamContext {
    pub fn new(
        model: impl Into<String>,
        prompt_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            completion_id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            model: model.into(),
            created: Utc::now().timestamp(),
            tool_name_map,
            thinking_enabled,
            tool_calls: HashMap::new(),
            next_tool_index: 0,
            has_tool_use: false,
            finish_reason: None,
            sent_role: false,
            prompt_tokens,
            completion_tokens: 0,
            text_buffer: String::new(),
            in_thinking: false,
            thinking_extracted: false,
            stream_failed: false,
        }
    }

    pub fn create_error_chunk(message: &str) -> OpenAiChunk {
        OpenAiChunk {
            data: json!({
                "error": {
                    "message": message,
                    "type": "server_error",
                    "code": "upstream_error"
                }
            }),
        }
    }

    fn base_chunk(&self) -> serde_json::Value {
        json!({
            "id": self.completion_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
        })
    }

    fn make_choice_chunk(&self, delta: serde_json::Value, finish_reason: Option<&str>) -> OpenAiChunk {
        OpenAiChunk {
            data: {
                let mut chunk = self.base_chunk();
                chunk["choices"] = json!([{
                    "index": 0,
                    "delta": delta,
                    "finish_reason": finish_reason
                }]);
                chunk
            },
        }
    }

    /// 首 chunk：带 role
    pub fn initial_chunk(&mut self) -> OpenAiChunk {
        self.sent_role = true;
        self.make_choice_chunk(json!({"role": "assistant", "content": ""}), None)
    }

    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<OpenAiChunk> {
        match event {
            Event::AssistantResponse(resp) => self.process_text(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ContextUsage(context_usage) => {
                let window_size = get_context_window_size(&self.model);
                self.prompt_tokens =
                    (context_usage.context_usage_percentage * (window_size as f64) / 100.0) as i32;
                if context_usage.context_usage_percentage >= 100.0 {
                    self.finish_reason = Some("length".to_string());
                }
                Vec::new()
            }
            Event::Exception { exception_type, .. } => {
                if exception_type == "ContentLengthExceededException" {
                    self.finish_reason = Some("length".to_string());
                }
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                self.stream_failed = true;
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                vec![Self::create_error_chunk(&format!(
                    "{error_code}: {error_message}"
                ))]
            }
            _ => Vec::new(),
        }
    }

    fn process_text(&mut self, content: &str) -> Vec<OpenAiChunk> {
        if content.is_empty() {
            return Vec::new();
        }

        self.completion_tokens += (content.len() as i32 + 3) / 4;

        if self.thinking_enabled {
            return self.process_text_with_thinking(content);
        }

        let mut chunks = Vec::new();
        if !self.sent_role {
            chunks.push(self.initial_chunk());
        }
        chunks.push(self.make_choice_chunk(json!({"content": content}), None));
        chunks
    }

    fn process_text_with_thinking(&mut self, content: &str) -> Vec<OpenAiChunk> {
        let mut chunks = Vec::new();
        self.text_buffer.push_str(content);

        loop {
            if !self.in_thinking && !self.thinking_extracted {
                if let Some(start) = self.text_buffer.find("<thinking>") {
                    let before = self.text_buffer[..start].to_string();
                    if !before.is_empty() {
                        if !self.sent_role {
                            chunks.push(self.initial_chunk());
                        }
                        chunks.push(self.make_choice_chunk(json!({"content": before}), None));
                    }
                    self.in_thinking = true;
                    self.text_buffer = self.text_buffer[start + "<thinking>".len()..].to_string();
                } else {
                    // 保留可能是部分标签的尾部
                    let keep = "<thinking>".len().min(self.text_buffer.len());
                    let flush_len = find_char_boundary(
                        &self.text_buffer,
                        self.text_buffer.len().saturating_sub(keep),
                    );
                    if flush_len > 0 {
                        let safe = self.text_buffer[..flush_len].to_string();
                        if !safe.trim().is_empty() {
                            if !self.sent_role {
                                chunks.push(self.initial_chunk());
                            }
                            chunks.push(self.make_choice_chunk(json!({"content": safe}), None));
                        }
                        self.text_buffer = self.text_buffer[flush_len..].to_string();
                    }
                    break;
                }
            } else if self.in_thinking {
                if let Some(end) = self.text_buffer.find("</thinking>") {
                    let thinking = self.text_buffer[..end]
                        .strip_prefix('\n')
                        .unwrap_or(&self.text_buffer[..end])
                        .to_string();
                    if !thinking.is_empty() {
                        if !self.sent_role {
                            chunks.push(self.initial_chunk());
                        }
                        chunks.push(
                            self.make_choice_chunk(json!({"reasoning_content": thinking}), None),
                        );
                    }
                    self.in_thinking = false;
                    self.thinking_extracted = true;
                    self.text_buffer = self.text_buffer[end + "</thinking>".len()..].to_string();
                    // 跳过 thinking 结束后的换行
                    if self.text_buffer.starts_with("\n\n") {
                        self.text_buffer = self.text_buffer[2..].to_string();
                    } else if self.text_buffer.starts_with('\n') {
                        self.text_buffer = self.text_buffer[1..].to_string();
                    }
                } else {
                    break;
                }
            } else {
                if !self.text_buffer.is_empty() {
                    let rest = std::mem::take(&mut self.text_buffer);
                    if !self.sent_role {
                        chunks.push(self.initial_chunk());
                    }
                    chunks.push(self.make_choice_chunk(json!({"content": rest}), None));
                }
                break;
            }
        }

        chunks
    }

    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<OpenAiChunk> {
        let mut chunks = Vec::new();
        self.has_tool_use = true;

        // flush 非 thinking 模式下可能残留的 text buffer
        if !self.thinking_enabled && !self.text_buffer.is_empty() {
            let buffered = std::mem::take(&mut self.text_buffer);
            if !self.sent_role {
                chunks.push(self.initial_chunk());
            }
            chunks.push(self.make_choice_chunk(json!({"content": buffered}), None));
        }

        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());

        let (index, id, name, is_new) = {
            if let Some(state) = self.tool_calls.get(&tool_use.tool_use_id) {
                (
                    state.index,
                    state.id.clone(),
                    state.name.clone(),
                    false,
                )
            } else {
                let index = self.next_tool_index;
                self.next_tool_index += 1;
                let id = tool_use.tool_use_id.clone();
                self.tool_calls.insert(
                    tool_use.tool_use_id.clone(),
                    ToolCallState {
                        index,
                        id: id.clone(),
                        name: original_name.clone(),
                        arguments: String::new(),
                        started: false,
                    },
                );
                (index, id, original_name, true)
            }
        };

        if is_new {
            if let Some(state) = self.tool_calls.get_mut(&tool_use.tool_use_id) {
                state.started = true;
            }
            if !self.sent_role {
                chunks.push(self.initial_chunk());
            }
            chunks.push(self.make_choice_chunk(
                json!({
                    "tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": ""
                        }
                    }]
                }),
                None,
            ));
        }

        if !tool_use.input.is_empty() {
            if let Some(state) = self.tool_calls.get_mut(&tool_use.tool_use_id) {
                state.arguments.push_str(&tool_use.input);
            }
            self.completion_tokens += (tool_use.input.len() as i32 + 3) / 4;
            chunks.push(self.make_choice_chunk(
                json!({
                    "tool_calls": [{
                        "index": index,
                        "function": {
                            "arguments": tool_use.input
                        }
                    }]
                }),
                None,
            ));
        }

        chunks
    }

    pub fn generate_final_chunks(&mut self, include_usage: bool) -> Vec<OpenAiChunk> {
        if self.stream_failed {
            return vec![OpenAiChunk {
                data: serde_json::Value::String("[DONE]".to_string()),
            }];
        }

        let mut chunks = Vec::new();

        // flush 剩余 buffer
        if self.thinking_enabled && !self.text_buffer.is_empty() {
            if self.in_thinking {
                let thinking = self.text_buffer.trim().to_string();
                if !thinking.is_empty() {
                    if !self.sent_role {
                        chunks.push(self.initial_chunk());
                    }
                    chunks.push(
                        self.make_choice_chunk(json!({"reasoning_content": thinking}), None),
                    );
                }
            } else if !self.text_buffer.trim().is_empty() {
                let rest = self.text_buffer.trim().to_string();
                if !self.sent_role {
                    chunks.push(self.initial_chunk());
                }
                chunks.push(
                    self.make_choice_chunk(json!({"content": rest}), None),
                );
            }
            self.text_buffer.clear();
        }

        let finish = self.finish_reason.clone().unwrap_or_else(|| {
            if self.has_tool_use {
                "tool_calls".to_string()
            } else {
                "stop".to_string()
            }
        });

        let mut final_chunk = self.base_chunk();
        final_chunk["choices"] = json!([{
            "index": 0,
            "delta": {},
            "finish_reason": finish
        }]);

        if include_usage {
            final_chunk["usage"] = json!({
                "prompt_tokens": self.prompt_tokens,
                "completion_tokens": self.completion_tokens.max(1),
                "total_tokens": self.prompt_tokens + self.completion_tokens.max(1)
            });
        }

        chunks.push(OpenAiChunk { data: final_chunk });
        chunks.push(OpenAiChunk {
            data: serde_json::Value::String("[DONE]".to_string()),
        });
        chunks
    }
}

/// 将 [DONE] 标记转为 SSE 字符串
pub fn done_sse() -> String {
    "data: [DONE]\n\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::events::ToolUseEvent;

    #[test]
    fn test_text_stream() {
        let mut ctx = OpenAiStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        let event: crate::kiro::model::events::AssistantResponseEvent =
            serde_json::from_str(r#"{"content":"Hello"}"#).unwrap();
        let chunks = ctx.process_kiro_event(&Event::AssistantResponse(event));
        assert!(!chunks.is_empty());
        let has_hello = chunks.iter().any(|c| {
            c.data["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap_or("")
                .contains("Hello")
        });
        assert!(has_hello);
    }

    #[test]
    fn test_tool_call_stream() {
        let mut ctx = OpenAiStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        let chunks = ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            tool_use_id: "call_abc".to_string(),
            name: "test_fn".to_string(),
            input: r#"{"a":1}"#.to_string(),
            stop: true,
        }));
        assert!(!chunks.is_empty());
        let final_chunks = ctx.generate_final_chunks(false);
        assert_eq!(
            final_chunks[0].data["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }
}

//! OpenAI Responses API 流式响应：Kiro 事件 → Responses SSE

use std::collections::HashMap;

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::anthropic::get_context_window_size;
use crate::kiro::model::events::Event;

use super::utf8::find_char_boundary;

/// Responses SSE 事件
#[derive(Debug, Clone)]
pub struct ResponsesSseEvent {
    pub event_type: String,
    pub data: Value,
}

impl ResponsesSseEvent {
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event_type,
            serde_json::to_string(&self.data).unwrap_or_else(|_| "{}".to_string())
        )
    }
}

struct ToolCallState {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    item_added: bool,
}

/// Responses 流式上下文
pub struct ResponsesStreamContext {
    response_id: String,
    model: String,
    created_at: i64,
    message_item_id: String,
    output_index: i32,
    content_index: i32,
    message_started: bool,
    text_part_started: bool,
    reasoning_started: bool,
    thinking_enabled: bool,
    tool_name_map: HashMap<String, String>,
    tool_calls: HashMap<String, ToolCallState>,
    next_output_index: i32,
    full_text: String,
    output_items: Vec<Value>,
    has_tool_use: bool,
    input_tokens: i32,
    output_tokens: i32,
    status: String,
    initialized: bool,
    text_buffer: String,
    in_thinking: bool,
    thinking_extracted: bool,
    reasoning_text: String,
}

impl ResponsesStreamContext {
    pub fn new(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            response_id: format!("resp_{}", Uuid::new_v4().simple()),
            model: model.into(),
            created_at: Utc::now().timestamp(),
            message_item_id: format!("msg_{}", Uuid::new_v4().simple()),
            output_index: 0,
            content_index: 0,
            message_started: false,
            text_part_started: false,
            reasoning_started: false,
            thinking_enabled,
            tool_name_map,
            tool_calls: HashMap::new(),
            next_output_index: 1,
            full_text: String::new(),
            output_items: Vec::new(),
            has_tool_use: false,
            input_tokens,
            output_tokens: 0,
            status: "in_progress".to_string(),
            initialized: false,
            text_buffer: String::new(),
            in_thinking: false,
            thinking_extracted: false,
            reasoning_text: String::new(),
        }
    }

    fn response_shell(&self) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": self.status,
            "model": self.model,
            "output": self.output_items.clone(),
        })
    }

    fn event(&self, event_type: &str, extra: Value) -> ResponsesSseEvent {
        let mut data = extra;
        if data.get("type").is_none() {
            if let Value::Object(ref mut map) = data {
                map.insert("type".to_string(), Value::String(event_type.to_string()));
            }
        }
        ResponsesSseEvent {
            event_type: event_type.to_string(),
            data,
        }
    }

    /// 流开始时的初始事件
    pub fn generate_initial_events(&mut self) -> Vec<ResponsesSseEvent> {
        if self.initialized {
            return Vec::new();
        }
        self.initialized = true;

        vec![
            self.event(
                "response.created",
                json!({
                    "type": "response.created",
                    "response": self.response_shell()
                }),
            ),
            self.event(
                "response.in_progress",
                json!({
                    "type": "response.in_progress",
                    "response": self.response_shell()
                }),
            ),
        ]
    }

    fn ensure_message_started(&mut self) -> Vec<ResponsesSseEvent> {
        if self.message_started {
            return Vec::new();
        }
        self.message_started = true;
        let mut events = vec![self.event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": self.output_index,
                "item": {
                    "id": self.message_item_id,
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": []
                }
            }),
        )];
        events.extend(self.ensure_text_part_started());
        events
    }

    fn ensure_text_part_started(&mut self) -> Vec<ResponsesSseEvent> {
        if self.text_part_started {
            return Vec::new();
        }
        self.text_part_started = true;
        vec![self.event(
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": self.message_item_id,
                "output_index": self.output_index,
                "content_index": self.content_index,
                "part": {
                    "type": "output_text",
                    "text": ""
                }
            }),
        )]
    }

    fn ensure_reasoning_started(&mut self) -> Vec<ResponsesSseEvent> {
        if self.reasoning_started || !self.thinking_enabled {
            return Vec::new();
        }
        self.reasoning_started = true;
        vec![self.event(
            "response.reasoning_summary_text.delta",
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": self.message_item_id,
                "output_index": self.output_index,
                "summary_index": 0,
                "delta": ""
            }),
        )]
    }

    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<ResponsesSseEvent> {
        match event {
            Event::AssistantResponse(resp) => self.process_text(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ContextUsage(cu) => {
                let window = get_context_window_size(&self.model);
                self.input_tokens =
                    (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                if cu.context_usage_percentage >= 100.0 {
                    self.status = "incomplete".to_string();
                }
                Vec::new()
            }
            Event::Exception { exception_type, .. } => {
                if exception_type == "ContentLengthExceededException" {
                    self.status = "incomplete".to_string();
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn process_text(&mut self, content: &str) -> Vec<ResponsesSseEvent> {
        if content.is_empty() {
            return Vec::new();
        }
        self.output_tokens += (content.len() as i32 + 3) / 4;

        if self.thinking_enabled {
            return self.process_text_with_thinking(content);
        }

        let mut events = self.ensure_message_started();
        self.full_text.push_str(content);
        events.push(self.event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "item_id": self.message_item_id,
                "output_index": self.output_index,
                "content_index": self.content_index,
                "delta": content
            }),
        ));
        events
    }

    fn process_text_with_thinking(&mut self, content: &str) -> Vec<ResponsesSseEvent> {
        let mut events = Vec::new();
        self.text_buffer.push_str(content);

        loop {
            if !self.in_thinking && !self.thinking_extracted {
                if let Some(start) = self.text_buffer.find("<thinking>") {
                    let before = self.text_buffer[..start].to_string();
                    if !before.is_empty() {
                        events.extend(self.ensure_message_started());
                        self.full_text.push_str(&before);
                        events.push(self.event(
                            "response.output_text.delta",
                            json!({
                                "type": "response.output_text.delta",
                                "item_id": self.message_item_id,
                                "output_index": self.output_index,
                                "content_index": self.content_index,
                                "delta": before
                            }),
                        ));
                    }
                    self.in_thinking = true;
                    self.text_buffer = self.text_buffer[start + "<thinking>".len()..].to_string();
                } else {
                    let keep = "<thinking>".len().min(self.text_buffer.len());
                    let flush_len = find_char_boundary(
                        &self.text_buffer,
                        self.text_buffer.len().saturating_sub(keep),
                    );
                    if flush_len > 0 {
                        let safe = self.text_buffer[..flush_len].to_string();
                        if !safe.trim().is_empty() {
                            events.extend(self.ensure_message_started());
                            self.full_text.push_str(&safe);
                            events.push(self.event(
                                "response.output_text.delta",
                                json!({
                                    "type": "response.output_text.delta",
                                    "item_id": self.message_item_id,
                                    "output_index": self.output_index,
                                    "content_index": self.content_index,
                                    "delta": safe
                                }),
                            ));
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
                        events.extend(self.ensure_reasoning_started());
                        self.reasoning_text.push_str(&thinking);
                        events.push(self.event(
                            "response.reasoning_summary_text.delta",
                            json!({
                                "type": "response.reasoning_summary_text.delta",
                                "item_id": self.message_item_id,
                                "output_index": self.output_index,
                                "summary_index": 0,
                                "delta": thinking
                            }),
                        ));
                    }
                    self.in_thinking = false;
                    self.thinking_extracted = true;
                    self.text_buffer = self.text_buffer[end + "</thinking>".len()..].to_string();
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
                    events.extend(self.ensure_message_started());
                    self.full_text.push_str(&rest);
                    events.push(self.event(
                        "response.output_text.delta",
                        json!({
                            "type": "response.output_text.delta",
                            "item_id": self.message_item_id,
                            "output_index": self.output_index,
                            "content_index": self.content_index,
                            "delta": rest
                        }),
                    ));
                }
                break;
            }
        }
        events
    }

    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<ResponsesSseEvent> {
        let mut events = Vec::new();
        self.has_tool_use = true;

        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());

        let (item_id, call_id, name, item_added) = {
            if let Some(state) = self.tool_calls.get(&tool_use.tool_use_id) {
                (
                    state.item_id.clone(),
                    state.call_id.clone(),
                    state.name.clone(),
                    state.item_added,
                )
            } else {
                let item_id = format!("fc_{}", Uuid::new_v4().simple());
                let call_id = tool_use.tool_use_id.clone();
                self.tool_calls.insert(
                    tool_use.tool_use_id.clone(),
                    ToolCallState {
                        item_id: item_id.clone(),
                        call_id: call_id.clone(),
                        name: original_name.clone(),
                        arguments: String::new(),
                        item_added: false,
                    },
                );
                (item_id, call_id, original_name, false)
            }
        };

        let output_index = if item_added {
            self.tool_calls
                .get(&tool_use.tool_use_id)
                .map(|_| self.output_index)
                .unwrap_or(self.next_output_index)
        } else {
            let idx = self.next_output_index;
            self.next_output_index += 1;
            idx
        };

        if !item_added {
            if let Some(state) = self.tool_calls.get_mut(&tool_use.tool_use_id) {
                state.item_added = true;
            }
            events.push(self.event(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {
                        "id": item_id,
                        "type": "function_call",
                        "status": "in_progress",
                        "call_id": call_id,
                        "name": name,
                        "arguments": ""
                    }
                }),
            ));
        }

        if !tool_use.input.is_empty() {
            if let Some(state) = self.tool_calls.get_mut(&tool_use.tool_use_id) {
                state.arguments.push_str(&tool_use.input);
            }
            self.output_tokens += (tool_use.input.len() as i32 + 3) / 4;
            events.push(self.event(
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": item_id,
                    "output_index": output_index,
                    "delta": tool_use.input
                }),
            ));
        }

        if tool_use.stop {
            let args = self
                .tool_calls
                .get(&tool_use.tool_use_id)
                .map(|s| s.arguments.clone())
                .unwrap_or_default();
            events.push(self.event(
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "arguments": args,
                    "name": name
                }),
            ));
            let fc_item = json!({
                "id": item_id,
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": name,
                "arguments": args
            });
            self.output_items.push(fc_item.clone());
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": fc_item
                }),
            ));
        }

        events
    }

    pub fn generate_final_events(&mut self) -> Vec<ResponsesSseEvent> {
        let mut events = Vec::new();

        // flush thinking buffer
        if self.thinking_enabled && !self.text_buffer.is_empty() {
            if self.in_thinking {
                let thinking = self.text_buffer.trim().to_string();
                if !thinking.is_empty() {
                    events.extend(self.ensure_reasoning_started());
                    self.reasoning_text.push_str(&thinking);
                    events.push(self.event(
                        "response.reasoning_summary_text.delta",
                        json!({
                            "type": "response.reasoning_summary_text.delta",
                            "item_id": self.message_item_id,
                            "output_index": self.output_index,
                            "summary_index": 0,
                            "delta": thinking
                        }),
                    ));
                }
            } else if !self.text_buffer.trim().is_empty() {
                let rest = self.text_buffer.trim().to_string();
                events.extend(self.ensure_message_started());
                self.full_text.push_str(&rest);
                events.push(self.event(
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": self.message_item_id,
                        "output_index": self.output_index,
                        "content_index": self.content_index,
                        "delta": rest
                    }),
                ));
            }
            self.text_buffer.clear();
        }

        if self.message_started && self.text_part_started {
            events.push(self.event(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done",
                    "item_id": self.message_item_id,
                    "output_index": self.output_index,
                    "content_index": self.content_index,
                    "text": self.full_text
                }),
            ));
            events.push(self.event(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done",
                    "item_id": self.message_item_id,
                    "output_index": self.output_index,
                    "content_index": self.content_index,
                    "part": {
                        "type": "output_text",
                        "text": self.full_text
                    }
                }),
            ));
            let msg_item = json!({
                "id": self.message_item_id,
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": self.full_text
                }]
            });
            if !self.full_text.is_empty() || !self.has_tool_use {
                self.output_items.insert(0, msg_item.clone());
            }
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": self.output_index,
                    "item": msg_item
                }),
            ));
        }

        if self.status != "incomplete" {
            self.status = "completed".to_string();
        }

        let usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens.max(1),
            "total_tokens": self.input_tokens + self.output_tokens.max(1)
        });

        let mut final_response = self.response_shell();
        final_response["usage"] = usage.clone();
        final_response["status"] = Value::String(self.status.clone());

        events.push(self.event(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": final_response
            }),
        ));

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_and_final_events() {
        let mut ctx = ResponsesStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        let initial = ctx.generate_initial_events();
        assert_eq!(initial.len(), 2);
        assert_eq!(initial[0].event_type, "response.created");

        let event: crate::kiro::model::events::AssistantResponseEvent =
            serde_json::from_str(r#"{"content":"Hi"}"#).unwrap();
        let deltas = ctx.process_kiro_event(&Event::AssistantResponse(event));
        assert!(!deltas.is_empty());

        let final_events = ctx.generate_final_events();
        assert!(
            final_events
                .iter()
                .any(|e| e.event_type == "response.completed")
        );
    }
}

//! OpenAI Responses API 流式响应：Kiro 事件 → Responses SSE

use std::collections::HashMap;

use chrono::Utc;
use serde_json::{Value, json};
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
    /// 是否已收到 `stop`（完整）。流结束时 `!finished` 且已开始输出即视为被截断。
    finished: bool,
    /// `output_item.added` 时分配的 output_index，截断收尾补 `output_item.done` 时复用。
    /// code mode 下 item 在 `stop` 时才分配 index，截断时为 `None`（从未发出任何事件，无需封口）。
    output_index: Option<i32>,
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
    pub stream_failed: bool,
    /// code mode：把子工具调用合成为 exec 的 custom_tool_call（内含 JS）回传给 codex。
    code_mode: bool,
    /// 模型用量是否已上报（防止多个 finalize 出口重复计数）
    usage_reported: bool,
    /// 本次流使用的上游凭据 id（-1 = 未知），供监控按凭据统计
    credential_id: i64,
    /// 本次请求上游计费的 credits 消耗（meteringEvent 累加）
    credits_used: f64,
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
            stream_failed: false,
            code_mode: false,
            usage_reported: false,
            credential_id: -1,
            credits_used: 0.0,
        }
    }

    /// 启用 code mode 输出转换（子工具调用 → exec custom_tool_call）。
    pub fn set_code_mode(&mut self, enabled: bool) {
        self.code_mode = enabled;
    }

    /// 设置本次流使用的上游凭据 id（供监控按凭据统计）
    pub fn set_credential_id(&mut self, id: u64) {
        self.credential_id = id as i64;
    }

    pub fn create_error_event(message: &str) -> ResponsesSseEvent {
        ResponsesSseEvent {
            event_type: "error".to_string(),
            data: json!({
                "type": "error",
                "error": {
                    "message": message,
                    "type": "server_error",
                    "code": "upstream_error"
                }
            }),
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
                self.input_tokens = (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                if cu.context_usage_percentage >= 100.0 {
                    self.status = "incomplete".to_string();
                }
                Vec::new()
            }
            Event::Metering(metering) => {
                if metering.usage > 0.0 {
                    self.credits_used += metering.usage;
                }
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                if exception_type == "ContentLengthExceededException" {
                    self.status = "incomplete".to_string();
                    return Vec::new();
                }
                self.stream_failed = true;
                self.status = "failed".to_string();
                tracing::error!("收到异常事件: {} - {}", exception_type, message);
                vec![Self::create_error_event(&format!(
                    "{exception_type}: {message}"
                ))]
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                self.stream_failed = true;
                self.status = "failed".to_string();
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                vec![Self::create_error_event(&format!(
                    "{error_code}: {error_message}"
                ))]
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

        // code mode：把子工具调用合成为 exec 的 custom_tool_call。
        // JS 需要完整参数，故仅缓冲，待 stop 时一次性发出 exec 事件序列。
        if self.code_mode {
            return self.process_tool_use_code_mode(tool_use, &original_name);
        }

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
                        finished: false,
                        output_index: None,
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
                state.output_index = Some(output_index);
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
            if let Some(state) = self.tool_calls.get_mut(&tool_use.tool_use_id) {
                state.finished = true;
            }
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

    /// code mode 下的工具输出：缓冲参数，stop 时合成一个 exec 的 custom_tool_call。
    /// 其 input 是 `await tools.<subtool>(<args>);` 形式的 JS，由 codex 本地 code-mode-host 执行。
    fn process_tool_use_code_mode(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
        sub_name: &str,
    ) -> Vec<ResponsesSseEvent> {
        // 缓冲参数（沿用 tool_calls 的 arguments 字段，name 记子工具名）
        let (item_id, call_id) = {
            if let Some(state) = self.tool_calls.get(&tool_use.tool_use_id) {
                (state.item_id.clone(), state.call_id.clone())
            } else {
                let item_id = format!("ctc_{}", Uuid::new_v4().simple());
                let call_id = format!("call_{}", Uuid::new_v4().simple());
                self.tool_calls.insert(
                    tool_use.tool_use_id.clone(),
                    ToolCallState {
                        item_id: item_id.clone(),
                        call_id: call_id.clone(),
                        name: sub_name.to_string(),
                        arguments: String::new(),
                        item_added: false,
                        finished: false,
                        output_index: None,
                    },
                );
                (item_id, call_id)
            }
        };

        if !tool_use.input.is_empty() {
            if let Some(state) = self.tool_calls.get_mut(&tool_use.tool_use_id) {
                state.arguments.push_str(&tool_use.input);
            }
            self.output_tokens += (tool_use.input.len() as i32 + 3) / 4;
        }

        if !tool_use.stop {
            return Vec::new();
        }

        if let Some(state) = self.tool_calls.get_mut(&tool_use.tool_use_id) {
            state.finished = true;
        }

        // stop：解析完整参数，生成 JS，发出 exec custom_tool_call 事件序列
        let args_raw = self
            .tool_calls
            .get(&tool_use.tool_use_id)
            .map(|s| s.arguments.clone())
            .unwrap_or_default();
        let args_val: Value = if args_raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&args_raw).unwrap_or_else(|_| json!({ "input": args_raw }))
        };
        let freeform = super::code_mode::is_freeform_subtool(sub_name);
        let js = super::code_mode::generate_exec_js(sub_name, &args_val, freeform);

        let output_index = self.next_output_index;
        self.next_output_index += 1;

        let mut events = Vec::new();
        let added_item = json!({
            "id": item_id,
            "type": "custom_tool_call",
            "status": "in_progress",
            "call_id": call_id,
            "name": super::code_mode::EXEC_TOOL_NAME,
            "input": ""
        });
        events.push(self.event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": added_item
            }),
        ));
        events.push(self.event(
            "response.custom_tool_call_input.delta",
            json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": item_id,
                "output_index": output_index,
                "delta": js
            }),
        ));
        events.push(self.event(
            "response.custom_tool_call_input.done",
            json!({
                "type": "response.custom_tool_call_input.done",
                "item_id": item_id,
                "output_index": output_index,
                "input": js
            }),
        ));
        let done_item = json!({
            "id": item_id,
            "type": "custom_tool_call",
            "status": "completed",
            "call_id": call_id,
            "name": super::code_mode::EXEC_TOOL_NAME,
            "input": js
        });
        self.output_items.push(done_item.clone());
        events.push(self.event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": done_item
            }),
        ));
        events
    }

    /// 处理被截断的工具调用：模型输出在参数 JSON 写完前被 `max_tokens` 截断
    /// （收到开始却无 `stop`）。
    ///
    /// - **普通 function_call**：`output_item.added` + 半截 `arguments.delta` 已透传给
    ///   客户端，但缺 `output_item.done`。这里无条件补一个 `done` 封口，避免客户端挂着
    ///   一个永不完成的 item（正确性修复，不受开关控制）。
    /// - **code mode**：exec 事件在 `stop` 时才一次性发出，截断时一个事件都没发过，
    ///   天然「丢弃」，无需封口。
    ///
    /// 若存在任一被截断的工具调用，把状态置为 `incomplete`，并在开关开启时追加一段
    /// 与 `/cc` 一致的纠正文本（提示模型分块写）。挂空 item 的封口无条件生效，纠正
    /// 文本受 `codexTruncationCorrection` 控制。
    fn finalize_truncated_tool_calls(&mut self) -> Vec<ResponsesSseEvent> {
        // 收集被截断的调用：未收到 stop 且已开始写参数
        let mut truncated: Vec<(String, Option<i32>)> = self
            .tool_calls
            .values()
            .filter(|s| !s.finished && !s.arguments.is_empty())
            .map(|s| (s.item_id.clone(), s.output_index))
            .collect();
        if truncated.is_empty() {
            return Vec::new();
        }
        // 输出顺序稳定：按 output_index 排序（code mode 的 None 排在最后）
        truncated.sort_by_key(|(_, idx)| idx.unwrap_or(i32::MAX));

        let mut events = Vec::new();

        // 普通 function_call：补 output_item.done 封口挂空的 item
        for (item_id, output_index) in &truncated {
            let Some(output_index) = output_index else {
                continue; // code mode：从未发出 added，无需封口
            };
            let args = self
                .tool_calls
                .values()
                .find(|s| &s.item_id == item_id)
                .map(|s| (s.call_id.clone(), s.name.clone(), s.arguments.clone()));
            let Some((call_id, name, args)) = args else {
                continue;
            };
            let fc_item = json!({
                "id": item_id,
                "type": "function_call",
                "status": "incomplete",
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

        tracing::warn!(
            model = %self.model,
            truncated_tool_count = truncated.len(),
            code_mode = self.code_mode,
            "codex：模型输出在工具参数写完前被截断，已封口不完整调用"
        );

        // 追加纠正文本（受开关控制）
        if super::codex_truncation_correction_enabled() {
            let correction =
                crate::anthropic::truncated_tool_correction(crate::anthropic::active_chunk_lines());
            events.extend(self.ensure_message_started());
            self.full_text.push_str(&correction);
            events.push(self.event(
                "response.output_text.delta",
                json!({
                    "type": "response.output_text.delta",
                    "item_id": self.message_item_id,
                    "output_index": self.output_index,
                    "content_index": self.content_index,
                    "delta": correction
                }),
            ));
        }

        // 标记为截断收尾语义
        self.status = "incomplete".to_string();

        events
    }

    /// 上报本次请求的模型累计用量（每请求恰好一次，多个 finalize 出口靠此 guard 去重）
    fn report_usage_once(&mut self) {
        if self.usage_reported {
            return;
        }
        self.usage_reported = true;
        let input = self.input_tokens as i64;
        let output = self.output_tokens.max(0) as i64;
        crate::model_stats::record(&self.model, input, output, self.credits_used);
        // codex 兼容路径 token 为估算值，无缓存读写明细，记 0；credits 取上游 metering
        crate::stats_db::record(
            &self.model,
            self.credential_id,
            input,
            output,
            0,
            0,
            self.credits_used,
        );
    }

    pub fn generate_final_events(&mut self) -> Vec<ResponsesSseEvent> {
        if self.stream_failed {
            return self.finalize_stream_on_failure();
        }
        self.report_usage_once();

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

        // 处理被截断（收到开始却无 stop）的工具调用：封口挂空 item + 追加纠正文本。
        events.extend(self.finalize_truncated_tool_calls());

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

    /// 流异常结束时发送 response.completed（status=failed）
    pub fn finalize_stream_on_failure(&mut self) -> Vec<ResponsesSseEvent> {
        self.report_usage_once();
        let mut events = Vec::new();
        if !self.initialized {
            events.extend(self.generate_initial_events());
        }

        self.status = "failed".to_string();

        let usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens.max(1),
            "total_tokens": self.input_tokens + self.output_tokens.max(1)
        });

        let mut final_response = self.response_shell();
        final_response["usage"] = usage;
        final_response["status"] = Value::String("failed".to_string());

        events.push(self.event(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": final_response
            }),
        ));

        events
    }

    /// 中途断流（已向客户端输出、无法透明重放）时的优雅收尾。
    ///
    /// 与 [`Self::finalize_stream_on_failure`] 的区别：**不发 error 事件、不置
    /// status=failed**，而是把已输出的内容以 `status="incomplete"` 正常封口
    /// （复用 `generate_final_events` 补齐 output_text.done / output_item.done /
    /// response.completed）。`incomplete` 是 Responses 协议里「输出被截断」的语义
    /// （与 `ContentLengthExceededException`、context 100% 同一状态），codex 客户端
    /// 据此把已生成部分当作被截断的有效回答续写，而不是判定整轮失败后重试整轮
    /// ——后者既多扣配额（见 kiro 按请求计费）又可能形成重试循环。
    ///
    /// 注意：Responses 端点的 initial 事件是无条件 chain 在流最前面的，客户端从一开始
    /// 就已收到，故不存在「首帧前可透明重放」的窗口，这里只做优雅收尾。
    pub fn finalize_stream_on_interrupt(&mut self) -> Vec<ResponsesSseEvent> {
        // stream_failed 保持 false，确保 generate_final_events 走正常封口而非 failure 分支
        self.status = "incomplete".to_string();
        self.generate_final_events()
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

    #[test]
    fn test_code_mode_tool_use_emits_exec_custom_tool_call() {
        let mut ctx = ResponsesStreamContext::new("gpt-5.6-terra", 10, false, HashMap::new());
        ctx.set_code_mode(true);

        // 模拟上游分块下发 apply_patch 调用
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch";
        let args = serde_json::json!({ "input": patch }).to_string();
        let ev1: crate::kiro::model::events::ToolUseEvent = serde_json::from_value(json!({
            "toolUseId": "tu_1", "name": "apply_patch", "input": args, "stop": false
        }))
        .unwrap();
        let out1 = ctx.process_kiro_event(&Event::ToolUse(ev1));
        assert!(out1.is_empty(), "未 stop 前不应发事件（缓冲中）");

        let ev2: crate::kiro::model::events::ToolUseEvent = serde_json::from_value(json!({
            "toolUseId": "tu_1", "name": "apply_patch", "input": "", "stop": true
        }))
        .unwrap();
        let out2 = ctx.process_kiro_event(&Event::ToolUse(ev2));

        // 应发出 custom_tool_call（name=exec）序列，且 input 是 JS
        let done = out2
            .iter()
            .find(|e| e.event_type == "response.output_item.done")
            .expect("应有 output_item.done");
        let item = &done.data["item"];
        assert_eq!(item["type"], "custom_tool_call");
        assert_eq!(item["name"], "exec");
        let js = item["input"].as_str().unwrap();
        assert!(js.starts_with("await tools.apply_patch("));
        assert!(js.contains("Begin Patch"));

        // 有 input.delta 事件
        assert!(
            out2.iter()
                .any(|e| e.event_type == "response.custom_tool_call_input.delta")
        );
    }

    #[test]
    fn test_code_mode_exec_command_wraps_text() {
        // 取值型子工具（exec_command）：JS 应用 text() 包裹返回值，模型才看得到输出
        let mut ctx = ResponsesStreamContext::new("gpt-5.6-terra", 10, false, HashMap::new());
        ctx.set_code_mode(true);
        let ev: crate::kiro::model::events::ToolUseEvent = serde_json::from_value(json!({
            "toolUseId": "tu_2", "name": "exec_command",
            "input": "{\"cmd\":\"free -h\"}", "stop": true
        }))
        .unwrap();
        let out = ctx.process_kiro_event(&Event::ToolUse(ev));
        let done = out
            .iter()
            .find(|e| e.event_type == "response.output_item.done")
            .expect("应有 output_item.done");
        let js = done.data["item"]["input"].as_str().unwrap();
        assert!(
            js.starts_with("text(await tools.exec_command("),
            "实际: {js}"
        );
        assert!(js.contains("free -h"));
    }

    #[test]
    fn test_non_code_mode_tool_use_stays_function_call() {
        let mut ctx = ResponsesStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        // 未启用 code mode
        let ev: crate::kiro::model::events::ToolUseEvent = serde_json::from_value(json!({
            "toolUseId": "tu_1", "name": "get_weather", "input": "{\"city\":\"SG\"}", "stop": true
        }))
        .unwrap();
        let out = ctx.process_kiro_event(&Event::ToolUse(ev));
        let done = out
            .iter()
            .find(|e| e.event_type == "response.output_item.done")
            .expect("应有 output_item.done");
        assert_eq!(done.data["item"]["type"], "function_call");
    }

    #[test]
    fn test_code_mode_truncated_tool_emits_correction_and_incomplete() {
        // code mode 下写文件参数被截断（收到开始、无 stop）：
        // 残缺 exec 从未发出（天然丢弃），最终应追加纠正文本 + status=incomplete
        let mut ctx = ResponsesStreamContext::new("gpt-5.6-terra", 10, false, HashMap::new());
        ctx.set_code_mode(true);

        let args = serde_json::json!({ "input": "*** Begin Patch\n*** Add File: a" }).to_string();
        let ev: crate::kiro::model::events::ToolUseEvent = serde_json::from_value(json!({
            "toolUseId": "tu_1", "name": "apply_patch", "input": args, "stop": false
        }))
        .unwrap();
        let out = ctx.process_kiro_event(&Event::ToolUse(ev));
        assert!(out.is_empty(), "缓冲中不发事件");

        // 流正常结束（Ok(None)），但 tu_1 从未收到 stop
        let final_events = ctx.generate_final_events();

        // 追加了纠正文本
        let has_correction = final_events.iter().any(|e| {
            e.event_type == "response.output_text.delta"
                && e.data["delta"]
                    .as_str()
                    .is_some_and(|s| s.contains("truncated"))
        });
        assert!(has_correction, "应追加截断纠正文本");

        // status=incomplete
        let completed = final_events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .expect("应有 response.completed");
        assert_eq!(completed.data["response"]["status"], json!("incomplete"));

        // code mode 残缺调用不应产生 custom_tool_call（从未发出）
        assert!(
            !final_events
                .iter()
                .any(|e| e.event_type == "response.custom_tool_call_input.delta"),
            "截断的 code mode 调用不应发出任何 exec 事件"
        );
    }

    #[test]
    fn test_function_call_truncated_closes_dangling_item() {
        // 普通 function_call 参数被截断：added + 半截 delta 已透传，
        // 收尾应补 output_item.done（status=incomplete）封口挂空 item
        let mut ctx = ResponsesStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        let ev: crate::kiro::model::events::ToolUseEvent = serde_json::from_value(json!({
            "toolUseId": "tu_1", "name": "Write", "input": "{\"path\":\"a.txt\",\"content\":\"line1", "stop": false
        }))
        .unwrap();
        let out = ctx.process_kiro_event(&Event::ToolUse(ev));
        // added + delta 已发
        assert!(
            out.iter()
                .any(|e| e.event_type == "response.output_item.added")
        );

        let final_events = ctx.generate_final_events();

        // 补了 function_call 的 output_item.done，status=incomplete
        let fc_done = final_events.iter().find(|e| {
            e.event_type == "response.output_item.done"
                && e.data["item"]["type"] == json!("function_call")
        });
        let fc_done = fc_done.expect("应补 function_call 的 output_item.done");
        assert_eq!(fc_done.data["item"]["status"], json!("incomplete"));
        // 参数保留半截原样
        assert_eq!(
            fc_done.data["item"]["arguments"],
            json!("{\"path\":\"a.txt\",\"content\":\"line1")
        );

        // 整体 status=incomplete
        let completed = final_events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .unwrap();
        assert_eq!(completed.data["response"]["status"], json!("incomplete"));
    }

    #[test]
    fn test_completed_tool_no_truncation_correction() {
        // 完整的工具调用（收到 stop）不应触发任何截断收尾
        let mut ctx = ResponsesStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        let ev: crate::kiro::model::events::ToolUseEvent = serde_json::from_value(json!({
            "toolUseId": "tu_1", "name": "get_weather", "input": "{\"city\":\"SG\"}", "stop": true
        }))
        .unwrap();
        ctx.process_kiro_event(&Event::ToolUse(ev));
        let final_events = ctx.generate_final_events();

        let has_correction = final_events.iter().any(|e| {
            e.event_type == "response.output_text.delta"
                && e.data["delta"]
                    .as_str()
                    .is_some_and(|s| s.contains("truncated"))
        });
        assert!(!has_correction, "完整调用不应有纠正文本");
        let completed = final_events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .unwrap();
        assert_eq!(completed.data["response"]["status"], json!("completed"));
    }

    #[test]
    fn test_finalize_stream_on_failure_emits_failed_completed() {
        let mut ctx = ResponsesStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        ctx.stream_failed = true;
        let events = ctx.finalize_stream_on_failure();
        assert!(events.iter().any(|e| e.event_type == "response.completed"));
        let completed = events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .unwrap();
        assert_eq!(completed.data["response"]["status"], json!("failed"));
    }

    #[test]
    fn test_generate_final_events_on_failure() {
        let mut ctx = ResponsesStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        ctx.stream_failed = true;
        let events = ctx.generate_final_events();
        assert!(events.iter().any(|e| e.event_type == "response.completed"));
    }

    /// 中途断流优雅收尾：不发 error，status=incomplete，正常封口
    #[test]
    fn test_finalize_on_interrupt_incomplete_no_error() {
        let mut ctx = ResponsesStreamContext::new("claude-sonnet-4-6", 10, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        // 先输出一段正文，模拟「已向客户端输出后中途断流」
        let ev: crate::kiro::model::events::AssistantResponseEvent =
            serde_json::from_str(r#"{"content":"partial answer"}"#).unwrap();
        let _ = ctx.process_kiro_event(&Event::AssistantResponse(ev));

        let events = ctx.finalize_stream_on_interrupt();

        assert!(
            !events.iter().any(|e| e.event_type == "error"),
            "优雅收尾不应发 error 事件，实际: {events:?}"
        );
        let completed = events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .expect("应有 response.completed");
        assert_eq!(
            completed.data["response"]["status"],
            json!("incomplete"),
            "中途断流收尾 status 应为 incomplete"
        );
        // 已输出的正文应被正常封口
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "response.output_text.done"),
            "应有 output_text.done 封口已输出正文，实际: {events:?}"
        );
    }
}

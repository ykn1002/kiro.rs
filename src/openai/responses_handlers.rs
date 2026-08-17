//! OpenAI Responses API Handler

use std::{
    convert::Infallible,
    time::{Duration, Instant},
};

use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use tokio::time::{Instant as TokioInstant, interval_at};
use uuid::Uuid;

use crate::anthropic::{AppState, conversion_error_parts, get_context_window_size};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::parser::error::ParseError;
use crate::token;

use super::handlers::{map_provider_error, override_thinking_from_model_name};
use super::responses_converter::responses_to_anthropic;
use super::responses_stream::ResponsesStreamContext;
use super::responses_types::ResponsesRequest;
use super::types::ErrorResponse;

fn append_responses_failure_tail(ctx: &mut ResponsesStreamContext, sse_parts: &mut Vec<String>) {
    for ev in ctx.finalize_stream_on_failure() {
        sse_parts.push(ev.to_sse_string());
    }
}

fn handle_responses_decode_failure(
    ctx: &mut ResponsesStreamContext,
    sse_parts: &mut Vec<String>,
    e: &ParseError,
) -> bool {
    if matches!(
        e,
        ParseError::TooManyErrors { .. } | ParseError::BufferOverflow { .. }
    ) {
        tracing::error!("解码器停止: {}", e);
        ctx.stream_failed = true;
        crate::metrics::inc_stream_decode_failure();
        let err = ResponsesStreamContext::create_error_event(&format!("Stream decode failed: {e}"));
        sse_parts.push(err.to_sse_string());
        true
    } else {
        tracing::warn!("解码事件失败: {}", e);
        false
    }
}

/// Code Mode 在首个完整 exec 前允许的透明重放次数。
const MAX_RESPONSES_STREAM_RESTARTS: usize = 2;
/// 为了能够撤销首个 exec 前的输出，暂存的 Responses SSE 最大字节数。
const MAX_RESPONSES_REPLAY_BUFFER_BYTES: usize = 1024 * 1024;
const RESPONSES_PING_INTERVAL: Duration = Duration::from_secs(25);

/// 重放上游请求并重建 Responses 流上下文所需的输入。
struct ResponsesStreamRestarter {
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    code_mode: bool,
    remaining: usize,
}

impl ResponsesStreamRestarter {
    fn build_ctx(&self) -> ResponsesStreamContext {
        let mut ctx = ResponsesStreamContext::new(
            &self.model,
            self.input_tokens,
            self.thinking_enabled,
            self.tool_name_map.clone(),
        );
        ctx.set_code_mode(self.code_mode);
        ctx
    }

    async fn restart(&mut self) -> Option<reqwest::Response> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        match self
            .provider
            .call_api_stream(&self.request_body, Some(&self.model))
            .await
        {
            Ok(response) => {
                crate::metrics::inc_stream_restarted();
                Some(response)
            }
            Err(error) => {
                tracing::error!("Responses 断流重连失败: {error}");
                None
            }
        }
    }
}

/// Code Mode 首个完整 exec 前的 SSE 暂存区。
///
/// 只有 `response.output_item.done` 且 item 为 `custom_tool_call` 才代表客户端即将执行
/// 工具；该事件一旦放行，就永久关闭重放窗口，避免重放导致副作用重复执行。
struct ResponsesReplayBuffer {
    pending: Vec<String>,
    pending_bytes: usize,
    replay_window_open: bool,
    complete_exec_released: bool,
}

impl ResponsesReplayBuffer {
    fn new(replay_window_open: bool) -> Self {
        Self {
            pending: Vec::new(),
            pending_bytes: 0,
            replay_window_open,
            complete_exec_released: false,
        }
    }

    fn can_restart(&self) -> bool {
        self.replay_window_open
    }

    fn complete_exec_released(&self) -> bool {
        self.complete_exec_released
    }

    fn push_events(
        &mut self,
        events: impl IntoIterator<Item = super::responses_stream::ResponsesSseEvent>,
    ) -> Vec<String> {
        let mut output = Vec::new();
        for event in events {
            let releases_window = event.event_type == "response.output_item.done"
                && event.data["item"]["type"].as_str() == Some("custom_tool_call");
            let sse = event.to_sse_string();
            if self.replay_window_open {
                self.pending_bytes += sse.len();
                self.pending.push(sse);
                if releases_window || self.pending_bytes > MAX_RESPONSES_REPLAY_BUFFER_BYTES {
                    self.replay_window_open = false;
                    self.complete_exec_released = releases_window;
                    output.append(&mut self.pending);
                    self.pending_bytes = 0;
                }
            } else {
                output.push(sse);
            }
        }
        output
    }

    fn release(&mut self) -> Vec<String> {
        self.replay_window_open = false;
        self.pending_bytes = 0;
        std::mem::take(&mut self.pending)
    }
}

struct ResponsesSseStreamState<S> {
    body: S,
    ctx: ResponsesStreamContext,
    decoder: EventStreamDecoder,
    replay: ResponsesReplayBuffer,
    restarter: ResponsesStreamRestarter,
    ping: tokio::time::Interval,
    started_at: Instant,
    finished: bool,
    pending_output: Vec<String>,
}

/// POST /v1/responses
pub async fn create_response(
    State(state): State<AppState>,
    JsonExtractor(payload): JsonExtractor<ResponsesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        stream = %payload.stream,
        input_count = %payload.input.len(),
        "Received POST /v1/responses request"
    );

    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "server_error",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    let mut anthropic_payload = match responses_to_anthropic(&payload) {
        Ok(p) => p,
        Err(e) => {
            let (error_type, message) = conversion_error_parts(&e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    override_thinking_from_model_name(&mut anthropic_payload);

    let conversion_result = match crate::anthropic::convert_responses_request(&anthropic_payload) {
        Ok(r) => r,
        Err(e) => {
            let (error_type, message) = conversion_error_parts(&e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    let kiro_request = KiroRequest::from_conversation_state(conversion_result.conversation_state);

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "server_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    let input_tokens = token::count_all_tokens(
        anthropic_payload.model.clone(),
        anthropic_payload.system.clone(),
        anthropic_payload.messages.clone(),
        anthropic_payload.tools.clone(),
    ) as i32;

    let thinking_enabled = anthropic_payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    // code mode：请求 input 里含 additional_tools（codex gpt-5.6 responses-lite）
    let code_mode = super::code_mode::request_uses_code_mode(&payload.input);
    if code_mode {
        tracing::info!("检测到 code mode 请求（additional_tools），启用 exec 桥接");
    }

    // Codex 恒为 stream=true；未指定时也走流式
    if payload.stream {
        handle_responses_stream(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            state.passthrough_retry_after,
            code_mode,
        )
        .await
    } else {
        handle_responses_non_stream(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            state.passthrough_retry_after,
            code_mode,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_responses_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    passthrough_retry_after: bool,
    code_mode: bool,
) -> Response {
    let restarter = ResponsesStreamRestarter {
        provider: provider.clone(),
        request_body: request_body.to_string(),
        model: model.to_string(),
        input_tokens,
        thinking_enabled,
        tool_name_map,
        code_mode,
        remaining: MAX_RESPONSES_STREAM_RESTARTS,
    };
    let response = match provider
        .call_api_stream(&restarter.request_body, Some(model))
        .await
    {
        Ok(r) => r,
        Err(e) => return map_provider_error(e, passthrough_retry_after),
    };

    let ctx = restarter.build_ctx();
    let stream = create_responses_sse_stream(response, ctx, restarter);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn create_responses_sse_stream(
    response: reqwest::Response,
    mut ctx: ResponsesStreamContext,
    restarter: ResponsesStreamRestarter,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let mut replay = ResponsesReplayBuffer::new(restarter.code_mode);
    let pending_output = replay.push_events(ctx.generate_initial_events());
    let init = ResponsesSseStreamState {
        body: response.bytes_stream(),
        ctx,
        decoder: EventStreamDecoder::new(),
        replay,
        restarter,
        ping: interval_at(
            TokioInstant::now() + RESPONSES_PING_INTERVAL,
            RESPONSES_PING_INTERVAL,
        ),
        started_at: Instant::now(),
        finished: false,
        pending_output,
    };

    stream::unfold(init, |mut state| async move {
        if state.finished {
            return None;
        }
        if !state.pending_output.is_empty() {
            let bytes = std::mem::take(&mut state.pending_output)
                .into_iter()
                .map(|part| Ok(Bytes::from(part)))
                .collect::<Vec<_>>();
            return Some((stream::iter(bytes), state));
        }

        tokio::select! {
            chunk_result = state.body.next() => match chunk_result {
                Some(Ok(chunk)) => {
                    let mut sse_parts = Vec::new();
                    let mut terminal = false;
                    if let Err(error) = state.decoder.feed(&chunk) {
                        terminal = handle_responses_decode_failure(
                            &mut state.ctx,
                            &mut sse_parts,
                            &error,
                        );
                    }

                    if !terminal {
                        for result in state.decoder.decode_iter() {
                            match result {
                                Ok(frame) => {
                                    if let Ok(event) = Event::from_frame(frame) {
                                        sse_parts.extend(state.replay.push_events(
                                            state.ctx.process_kiro_event(&event),
                                        ));
                                    }
                                }
                                Err(error) => {
                                    if handle_responses_decode_failure(
                                        &mut state.ctx,
                                        &mut sse_parts,
                                        &error,
                                    ) {
                                        terminal = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    if terminal {
                        let can_restart = state.replay.can_restart();
                        let mut output = if can_restart {
                            // 解码器已不可恢复，但客户端仍未见到协议事件。丢弃暂存的
                            // 文本/推理，使用新的 response ID 发送干净的 initial + error。
                            state.replay.release();
                            state.ctx = state.restarter.build_ctx();
                            state
                                .ctx
                                .generate_initial_events()
                                .into_iter()
                                .map(|event| event.to_sse_string())
                                .collect()
                        } else {
                            state.replay.release()
                        };
                        output.append(&mut sse_parts);
                        append_responses_failure_tail(&mut state.ctx, &mut output);
                        state.finished = true;
                        let bytes: Vec<Result<Bytes, Infallible>> = output
                            .into_iter()
                            .map(|part| Ok::<Bytes, Infallible>(Bytes::from(part)))
                            .collect();
                        Some((stream::iter(bytes), state))
                    } else {
                        let bytes: Vec<Result<Bytes, Infallible>> = sse_parts
                            .into_iter()
                            .map(|part| Ok::<Bytes, Infallible>(Bytes::from(part)))
                            .collect();
                        Some((stream::iter(bytes), state))
                    }
                }
                Some(Err(error)) => {
                    crate::metrics::inc_stream_interrupted();
                    if state.replay.can_restart()
                        && !error.is_timeout()
                        && let Some(response) = state.restarter.restart().await
                    {
                        tracing::warn!(
                            "Responses 上游断流且首个完整 exec 前尚未输出，重放请求（剩余 {} 次，距流建立 {}ms）: {}",
                            state.restarter.remaining,
                            state.started_at.elapsed().as_millis(),
                            error
                        );
                        state.body = response.bytes_stream();
                        state.ctx = state.restarter.build_ctx();
                        state.decoder = EventStreamDecoder::new();
                        state.replay = ResponsesReplayBuffer::new(state.restarter.code_mode);
                        state.pending_output = state.replay.push_events(state.ctx.generate_initial_events());
                        state.ping = interval_at(
                            TokioInstant::now() + RESPONSES_PING_INTERVAL,
                            RESPONSES_PING_INTERVAL,
                        );
                        state.started_at = Instant::now();
                        Some((stream::iter(Vec::<Result<Bytes, Infallible>>::new()), state))
                    } else {
                        let can_restart = state.replay.can_restart();
                        let complete_exec_released = state.replay.complete_exec_released();
                        if can_restart {
                            tracing::error!(
                                "Responses 上游断流且首个完整 exec 前重放未兜住（剩余 {} 次，距流建立 {}ms）: {:?}",
                                state.restarter.remaining,
                                state.started_at.elapsed().as_millis(),
                                error
                            );
                            // 客户端尚未见到任何协议事件。丢弃前几轮的半截文本和旧
                            // response ID，生成干净的 initial + error 序列。
                            state.replay.release();
                            state.ctx = state.restarter.build_ctx();
                            let mut output = state.replay.push_events(state.ctx.generate_initial_events());
                            output.extend(state.replay.release());
                            state.ctx.stream_failed = true;
                            output.push(
                                ResponsesStreamContext::create_error_event(&format!(
                                    "Upstream stream error: {error}"
                                )).to_sse_string(),
                            );
                            append_responses_failure_tail(&mut state.ctx, &mut output);
                            state.pending_output = output;
                        } else if error.is_timeout() {
                            tracing::error!("读取 Responses 响应流失败（本地超时）: {error:?}");
                            let mut output = state.replay.release();
                            state.ctx.stream_failed = true;
                            output.push(
                                ResponsesStreamContext::create_error_event(&format!(
                                    "Upstream stream error: {error}"
                                )).to_sse_string(),
                            );
                            append_responses_failure_tail(&mut state.ctx, &mut output);
                            state.pending_output = output;
                        } else if state.restarter.code_mode && complete_exec_released {
                            // 完整 exec 已经交给 Codex，本地 code-mode-host 可以执行并
                            // 回传工具结果。此时发送 response.completed(incomplete) 会让
                            // Codex 直接终止；直接 EOF 给客户端一个继续提交工具结果的机会。
                            tracing::warn!(
                                "Responses 上游断流（完整 exec 已放行，直接结束 SSE 等待工具结果）: {error:?}"
                            );
                        } else {
                            tracing::warn!("Responses 上游中途断流（已输出正文，优雅收尾 incomplete）: {error:?}");
                            let mut output = state.replay.release();
                            output.extend(
                                state.ctx.finalize_stream_on_interrupt()
                                    .into_iter()
                                    .map(|event| event.to_sse_string()),
                            );
                            state.pending_output = output;
                        }
                        state.finished = true;
                        let bytes: Vec<Result<Bytes, Infallible>> = std::mem::take(&mut state.pending_output)
                            .into_iter()
                            .map(|part| Ok::<Bytes, Infallible>(Bytes::from(part)))
                            .collect();
                        Some((stream::iter(bytes), state))
                    }
                }
                None => {
                    let mut output = state.replay.push_events(state.ctx.generate_final_events());
                    output.extend(state.replay.release());
                    state.finished = true;
                    let bytes: Vec<Result<Bytes, Infallible>> = output
                        .into_iter()
                        .map(|part| Ok::<Bytes, Infallible>(Bytes::from(part)))
                        .collect();
                    Some((stream::iter(bytes), state))
                }
            },
            _ = state.ping.tick(), if state.restarter.code_mode => {
                Some((stream::iter(vec![Ok(Bytes::from(": ping\n\n"))]), state))
            }
        }
    })
    .flatten()
}

#[allow(clippy::too_many_arguments)]
async fn handle_responses_non_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    passthrough_retry_after: bool,
    code_mode: bool,
) -> Response {
    let response = match provider.call_api(request_body, Some(model)).await {
        Ok(r) => r,
        Err(e) => return map_provider_error(e, passthrough_retry_after),
    };

    let body_bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "server_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    let mut decoder = EventStreamDecoder::new();
    let _ = decoder.feed(&body_bytes);

    let mut text_content = String::new();
    let mut tool_items: Vec<serde_json::Value> = Vec::new();
    let mut tool_buffers: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut status = "completed".to_string();
    let mut prompt_tokens = input_tokens;

    for result in decoder.decode_iter() {
        if let Ok(frame) = result {
            if let Ok(event) = Event::from_frame(frame) {
                match event {
                    Event::AssistantResponse(resp) => text_content.push_str(&resp.content),
                    Event::ToolUse(tool_use) => {
                        let entry = tool_buffers
                            .entry(tool_use.tool_use_id.clone())
                            .or_insert_with(|| {
                                let name = tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());
                                (name, String::new())
                            });
                        entry.1.push_str(&tool_use.input);
                        if tool_use.stop {
                            let (name, args) = tool_buffers
                                .remove(&tool_use.tool_use_id)
                                .unwrap_or_default();
                            if code_mode {
                                // 合成 exec custom_tool_call（JS）
                                let args_val: serde_json::Value = if args.trim().is_empty() {
                                    json!({})
                                } else {
                                    serde_json::from_str(&args)
                                        .unwrap_or_else(|_| json!({ "input": args }))
                                };
                                let freeform = super::code_mode::is_freeform_subtool(&name);
                                let js =
                                    super::code_mode::generate_exec_js(&name, &args_val, freeform);
                                tool_items.push(json!({
                                    "id": format!("ctc_{}", Uuid::new_v4().simple()),
                                    "type": "custom_tool_call",
                                    "status": "completed",
                                    "call_id": format!("call_{}", Uuid::new_v4().simple()),
                                    "name": super::code_mode::EXEC_TOOL_NAME,
                                    "input": js
                                }));
                            } else {
                                tool_items.push(json!({
                                    "id": format!("fc_{}", Uuid::new_v4().simple()),
                                    "type": "function_call",
                                    "status": "completed",
                                    "call_id": tool_use.tool_use_id,
                                    "name": name,
                                    "arguments": args
                                }));
                            }
                        }
                    }
                    Event::ContextUsage(cu) => {
                        let window = get_context_window_size(model);
                        prompt_tokens =
                            (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                        if cu.context_usage_percentage >= 100.0 {
                            status = "incomplete".to_string();
                        }
                    }
                    Event::Error {
                        error_code,
                        error_message,
                    } => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(ErrorResponse::new(
                                "server_error",
                                format!("{error_code}: {error_message}"),
                            )),
                        )
                            .into_response();
                    }
                    Event::Exception {
                        exception_type,
                        message,
                    } => {
                        if exception_type == "ContentLengthExceededException" {
                            status = "incomplete".to_string();
                        } else {
                            return (
                                StatusCode::BAD_GATEWAY,
                                Json(ErrorResponse::new(
                                    "server_error",
                                    format!("{exception_type}: {message}"),
                                )),
                            )
                                .into_response();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let mut reasoning_text: Option<String> = None;
    let mut content_text = text_content.clone();
    if thinking_enabled {
        let (reasoning, remaining) =
            crate::anthropic::extract_thinking_from_complete_text(&text_content);
        reasoning_text = reasoning;
        content_text = remaining;
    } else if content_text.contains("<thinking>") {
        let (_, remaining) = crate::anthropic::extract_thinking_from_complete_text(&text_content);
        content_text = remaining;
    }

    let mut output = Vec::new();
    if let Some(reasoning) = reasoning_text.filter(|s| !s.is_empty()) {
        output.push(json!({
            "id": format!("rs_{}", Uuid::new_v4().simple()),
            "type": "reasoning",
            "status": "completed",
            "summary": [{"type": "summary_text", "text": reasoning}]
        }));
    }
    if !content_text.is_empty() || tool_items.is_empty() {
        output.push(json!({
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": content_text
            }]
        }));
    }
    output.extend(tool_items);

    let completion_tokens = token::estimate_output_tokens(&[json!({
        "type": "text",
        "text": text_content
    })]);

    let resp = json!({
        "id": format!("resp_{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": status,
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": prompt_tokens,
            "output_tokens": completion_tokens.max(1),
            "total_tokens": prompt_tokens + completion_tokens.max(1)
        }
    });

    (StatusCode::OK, Json(resp)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(
        event_type: &str,
        data: serde_json::Value,
    ) -> super::super::responses_stream::ResponsesSseEvent {
        super::super::responses_stream::ResponsesSseEvent {
            event_type: event_type.to_string(),
            data,
        }
    }

    #[test]
    fn code_mode_buffers_initial_text_and_reasoning_before_first_exec() {
        let mut buffer = ResponsesReplayBuffer::new(true);
        let output = buffer.push_events([
            event("response.created", json!({"type": "response.created"})),
            event(
                "response.in_progress",
                json!({"type": "response.in_progress"}),
            ),
            event(
                "response.reasoning_summary_text.delta",
                json!({"delta": "thinking"}),
            ),
            event("response.output_text.delta", json!({"delta": "draft"})),
        ]);

        assert!(output.is_empty());
        assert!(buffer.can_restart());
        let buffered = buffer.release();
        assert_eq!(buffered.len(), 4);
        assert!(buffered.iter().any(|s| s.contains("draft")));
    }

    #[test]
    fn first_completed_exec_releases_buffer_and_closes_restart_window() {
        let mut buffer = ResponsesReplayBuffer::new(true);
        assert!(
            buffer
                .push_events([event(
                    "response.output_text.delta",
                    json!({"delta": "draft"})
                )])
                .is_empty()
        );

        let output = buffer.push_events([event(
            "response.output_item.done",
            json!({"item": {"type": "custom_tool_call", "status": "completed"}}),
        )]);

        assert_eq!(output.len(), 2);
        assert!(output[0].contains("draft"));
        assert!(output[1].contains("custom_tool_call"));
        assert!(!buffer.can_restart());
        assert!(buffer.complete_exec_released());
        let later = buffer.push_events([event(
            "response.output_text.delta",
            json!({"delta": "after"}),
        )]);
        assert_eq!(later.len(), 1);
        assert!(later[0].contains("after"));
    }

    #[test]
    fn non_exec_output_item_does_not_close_restart_window() {
        let mut buffer = ResponsesReplayBuffer::new(true);
        assert!(
            buffer
                .push_events([event(
                    "response.output_item.done",
                    json!({"item": {"type": "message", "status": "completed"}}),
                )])
                .is_empty()
        );
        assert!(buffer.can_restart());
    }

    #[test]
    fn buffer_limit_releases_output_and_closes_restart_window() {
        let mut buffer = ResponsesReplayBuffer::new(true);
        let output = buffer.push_events([event(
            "response.output_text.delta",
            json!({"delta": "x".repeat(MAX_RESPONSES_REPLAY_BUFFER_BYTES)}),
        )]);

        assert_eq!(output.len(), 1);
        assert!(!buffer.can_restart());
        assert!(!buffer.complete_exec_released());
    }
}

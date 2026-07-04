//! Anthropic API Handler 函数

use std::convert::Infallible;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::parser::error::ParseError;
use crate::token;
use anyhow::Error;
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
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{convert_request, conversion_error_parts};
use super::middleware::AppState;
use super::stream::{SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }

    // 上游 HTTP 错误：对客户端可正确处理的状态码做透传（429 限流让客户端退避重试、
    // 402 额度耗尽让客户端感知），其余仍按 502 处理，不向客户端暴露凭据/权限细节。
    if let Some(api_err) = err.downcast_ref::<crate::kiro::provider::UpstreamApiError>() {
        match api_err.status {
            429 => {
                tracing::warn!(error = %err, "限流，透传 429 给客户端");
                let mut resp = (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(ErrorResponse::new(
                        "rate_limit_error",
                        "Rate limit reached. Please retry after the indicated delay.",
                    )),
                )
                    .into_response();
                if let Some(ra) = api_err.retry_after {
                    if let Ok(value) = header::HeaderValue::from_str(&ra.as_secs().max(1).to_string())
                    {
                        resp.headers_mut().insert(header::RETRY_AFTER, value);
                    }
                }
                return resp;
            }
            402 => {
                tracing::warn!(error = %err, "上游额度耗尽，透传 402 给客户端");
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(ErrorResponse::new(
                        "api_error",
                        "Upstream credential quota exhausted.",
                    )),
                )
                    .into_response();
            }
            _ => {}
        }
    }

    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

/// GET /metrics — Prometheus 文本格式指标（无需认证）
pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let (available, total) = match &state.kiro_provider {
        Some(p) => (p.available_credentials(), p.total_credentials()),
        None => (0, 0),
    };
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        crate::metrics::METRICS.render_prometheus(available, total),
    )
}

/// GET /healthz — 进程存活探针（无需认证）
pub async fn healthz() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// GET /readyz — 就绪探针：至少有一个未禁用的凭据（无需认证）
pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    match &state.kiro_provider {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not_ready",
                "reason": "kiro_provider_not_configured"
            })),
        )
            .into_response(),
        Some(provider) => {
            let available = provider.available_credentials();
            let total = provider.total_credentials();
            if available == 0 {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "status": "not_ready",
                        "reason": "no_available_credentials",
                        "total": total,
                        "available": available
                    })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    Json(json!({
                        "status": "ready",
                        "total": total,
                        "available": available
                    })),
                )
                    .into_response()
            }
        }
    }
}

/// GET /v1/models
///
/// 返回可用的模型列表。
///
/// 列表由模型注册表（配置 `models` 或内置默认表）驱动：每个模型定义自动派生
/// 基础变体与 `-thinking` 变体两条记录。
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let mut models = Vec::new();
    for def in super::converter::registered_models().iter() {
        // 基础变体
        models.push(Model {
            id: def.display_id.clone(),
            object: "model".to_string(),
            created: def.created,
            created_at: def.created,
            owned_by: "anthropic".to_string(),
            display_name: def.display_name.clone(),
            model_type: "model".to_string(),
            max_tokens: def.max_tokens,
        });
        // thinking 变体
        models.push(Model {
            id: format!("{}-thinking", def.display_id),
            object: "model".to_string(),
            created: def.created,
            created_at: def.created,
            owned_by: "anthropic".to_string(),
            display_name: format!("{} (Thinking)", def.display_name),
            model_type: "model".to_string(),
            max_tokens: def.max_tokens,
        });
    }

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    JsonExtractor(payload): JsonExtractor<MessagesRequest>,
) -> Response {
    handle_messages(state, payload, false, "/v1/messages").await
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点：等待 contextUsageEvent 后再发送 message_start。
pub async fn post_messages_cc(
    State(state): State<AppState>,
    JsonExtractor(payload): JsonExtractor<MessagesRequest>,
) -> Response {
    handle_messages(state, payload, true, "/cc/v1/messages").await
}

/// /v1 与 /cc/v1 共享的消息处理逻辑
async fn handle_messages(
    state: AppState,
    mut payload: MessagesRequest,
    delay_message_start: bool,
    log_path: &str,
) -> Response {
    tracing::info!(
        path = log_path,
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received messages request"
    );

    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    override_thinking_from_model_name(&mut payload);

    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = conversion_error_parts(&e);
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let model = payload.model;

    if payload.stream {
        handle_stream_request(
            provider,
            &request_body,
            &model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            delay_message_start,
        )
        .await
    } else {
        handle_non_stream_request(
            provider,
            &request_body,
            &model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    delay_message_start: bool,
) -> Response {
    let response = match provider.call_api_stream(request_body, Some(model)).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    let ctx = StreamContext::new_with_thinking(
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        delay_message_start,
    );

    let stream = create_sse_stream(response, ctx);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 流式解码错误处理：连续失败时标记流失败并向前端发送 error 事件
fn handle_stream_decode_error(
    events: &mut Vec<SseEvent>,
    ctx: &mut StreamContext,
    e: &ParseError,
) {
    if matches!(e, ParseError::TooManyErrors { .. }) {
        tracing::error!("解码器停止: {}", e);
        ctx.stream_failed = true;
        crate::metrics::inc_stream_decode_failure();
        events.push(StreamContext::create_error_event(&format!(
            "Stream decode failed: {e}"
        )));
    } else {
        tracing::warn!("解码事件失败: {}", e);
    }
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
            if finished {
                return None;
            }

            tokio::select! {
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            events.extend(ctx.take_events_for_kiro(&event));
                                        }
                                    }
                                    Err(e) => {
                                        handle_stream_decode_error(&mut events, &mut ctx, &e);
                                    }
                                }
                            }

                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            let stream_failed = ctx.stream_failed;
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, stream_failed, ping_interval)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {:?}", e);
                            ctx.stream_failed = true;
                            let err = StreamContext::create_error_event(&format!(
                                "Upstream stream error: {e}"
                            ));
                            let bytes = vec![Ok(Bytes::from(err.to_sse_string()))];
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                        None => {
                            let final_events = if ctx.stream_failed {
                                Vec::new()
                            } else {
                                ctx.finalize_stream()
                            };
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                    }
                }
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
                }
            }
        },
    )
    .flatten();

    processing_stream
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api(request_body, Some(model)).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

                            // 累积工具的 JSON 输入
                            let buffer = tool_json_buffers
                                .entry(tool_use.tool_use_id.clone())
                                .or_insert_with(String::new);
                            buffer.push_str(&tool_use.input);

                            // 如果是完整的工具调用，添加到列表
                            if tool_use.stop {
                                let input: serde_json::Value = if buffer.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    serde_json::from_str(buffer).unwrap_or_else(|e| {
                                        tracing::warn!(
                                            "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                                            e,
                                            tool_use.tool_use_id
                                        );
                                        serde_json::json!({})
                                    })
                                };

                                let original_name = tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use.tool_use_id,
                                    "name": original_name,
                                    "input": input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens =
                                (context_usage.context_usage_percentage * (window_size as f64)
                                    / 100.0) as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Error {
                            error_code,
                            error_message,
                        } => {
                            return (
                                StatusCode::BAD_GATEWAY,
                                Json(ErrorResponse::new(
                                    "api_error",
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
                                stop_reason = "max_tokens".to_string();
                            } else {
                                return (
                                    StatusCode::BAD_GATEWAY,
                                    Json(ErrorResponse::new(
                                        "api_error",
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
            Err(e) => {
                if matches!(e, ParseError::TooManyErrors { .. }) {
                    tracing::error!("非流式解码器停止: {}", e);
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(ErrorResponse::new(
                            "api_error",
                            format!("Upstream stream decode failed: {e}"),
                        )),
                    )
                        .into_response();
                }
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        // 从完整文本中提取 thinking 块
        let (thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(thinking_text) = thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text,
                "signature": super::stream::compute_thinking_signature(&thinking_text)
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
    let final_input_tokens = context_input_tokens.unwrap_or(input_tokens);

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": final_input_tokens,
            "output_tokens": output_tokens
        }
    });

    (StatusCode::OK, Json(response_body)).into_response()
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6 {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::provider::UpstreamApiError;

    #[test]
    fn test_map_provider_error_passes_through_429() {
        let err = anyhow::Error::new(UpstreamApiError {
            status: 429,
            message: "流式 API 请求失败: 429 Too Many Requests".to_string(),
            retry_after: Some(Duration::from_secs(30)),
        });
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("30")
        );
    }

    #[test]
    fn test_map_provider_error_local_rpm_429_includes_retry_after() {
        let err = anyhow::Error::new(UpstreamApiError {
            status: 429,
            message: "local credential RPM limit exceeded (8/min)".to_string(),
            retry_after: Some(Duration::from_secs(12)),
        });
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("12")
        );
    }

    #[test]
    fn test_map_provider_error_passes_through_402() {
        let err = anyhow::Error::new(UpstreamApiError {
            status: 402,
            message: "流式 API 请求失败（所有凭据已用尽）: 402".to_string(),
            retry_after: None,
        });
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[test]
    fn test_map_provider_error_other_upstream_status_is_502() {
        // 401/403 等凭据/权限类状态不应透传，仍按 502 处理
        let err = anyhow::Error::new(UpstreamApiError {
            status: 403,
            message: "流式 API 请求失败: 403 Forbidden".to_string(),
            retry_after: None,
        });
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn test_map_provider_error_plain_error_is_502() {
        let err = anyhow::anyhow!("网络发送失败");
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn test_map_provider_error_context_full_is_400() {
        let err = anyhow::anyhow!("xxx CONTENT_LENGTH_EXCEEDS_THRESHOLD yyy");
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

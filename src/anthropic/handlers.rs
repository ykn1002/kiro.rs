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

use super::converter::{conversion_error_parts, convert_request};
use super::middleware::AppState;
use super::stream::{SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error, passthrough_retry_after: bool) -> Response {
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
                crate::common::provider_error::insert_retry_after_header(
                    &mut resp,
                    api_err.retry_after,
                    passthrough_retry_after,
                );
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
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
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

    let kiro_request = KiroRequest::from_conversation_state(conversion_result.conversation_state);

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
            request_body,
            &model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            delay_message_start,
            state.passthrough_retry_after,
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
            state.passthrough_retry_after,
        )
        .await
    }
}

/// 上游断流后允许的透明重连次数
///
/// 大请求体（长对话）下的典型失败形态：上游在返回首个字节前思考数分钟，
/// 中间链路按空闲超时切断连接，hyper 报 `UnexpectedEof`。这类断开发生在
/// 我们向客户端发出任何 SSE 之前，可以原样重放请求，客户端无感知。
const MAX_STREAM_RESTARTS: usize = 2;

/// 断流重连所需的全部输入：重放上游请求并重建 [`StreamContext`]
///
/// 只在「尚未向客户端输出任何事件」时使用——已经发出 `message_start` 之后
/// 无法重来（协议不允许第二个 `message_start`，且已输出的内容无法撤回）。
struct StreamRestarter {
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    delay_message_start: bool,
    /// 剩余可用的重连次数
    remaining: usize,
}

impl StreamRestarter {
    /// 构建一个全新的流上下文（重连后旧上下文的 SSE 状态必须丢弃）
    fn build_ctx(&self) -> StreamContext {
        StreamContext::new_with_thinking(
            self.model.as_str(),
            self.input_tokens,
            self.thinking_enabled,
            self.tool_name_map.clone(),
            self.delay_message_start,
        )
    }

    /// 重放上游请求；次数用尽或重放失败返回 None
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
            Ok(resp) => {
                crate::metrics::inc_stream_restarted();
                Some(resp)
            }
            Err(e) => {
                tracing::error!("断流重连失败: {}", e);
                None
            }
        }
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    delay_message_start: bool,
    passthrough_retry_after: bool,
) -> Response {
    let restarter = StreamRestarter {
        provider: provider.clone(),
        request_body,
        model: model.to_string(),
        input_tokens,
        thinking_enabled,
        tool_name_map,
        delay_message_start,
        remaining: MAX_STREAM_RESTARTS,
    };

    let response = match provider
        .call_api_stream(&restarter.request_body, Some(model))
        .await
    {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e, passthrough_retry_after),
    };

    let ctx = restarter.build_ctx();

    let stream = create_sse_stream(response, ctx, restarter);

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
fn handle_stream_decode_error(events: &mut Vec<SseEvent>, ctx: &mut StreamContext, e: &ParseError) {
    if matches!(
        e,
        ParseError::TooManyErrors { .. } | ParseError::BufferOverflow { .. }
    ) {
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

/// SSE 转换状态机的可变状态
///
/// `body` 泛型化而非装箱：重连时赋的新值来自同一个
/// `reqwest::Response::bytes_stream()`，类型完全相同。
struct SseStreamState<S> {
    /// 上游响应体字节流
    body: S,
    /// Anthropic SSE 转换上下文
    ctx: StreamContext,
    /// event-stream 帧解码器
    decoder: EventStreamDecoder,
    /// 是否已终止（不再拉取上游）
    finished: bool,
    /// ping 保活定时器
    ping: tokio::time::Interval,
    /// 断流重连器
    restarter: StreamRestarter,
    /// 流建立（拿到 200 响应头）的时刻，用于诊断断流距开始的耗时
    started_at: std::time::Instant,
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    restarter: StreamRestarter,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let init = SseStreamState {
        body: response.bytes_stream(),
        ctx,
        decoder: EventStreamDecoder::new(),
        finished: false,
        ping: interval(Duration::from_secs(PING_INTERVAL_SECS)),
        restarter,
        started_at: std::time::Instant::now(),
    };

    let processing_stream = stream::unfold(init, |state| async move {
        let SseStreamState {
            mut body,
            mut ctx,
            mut decoder,
            finished,
            mut ping,
            mut restarter,
            started_at,
        } = state;

        if finished {
            return None;
        }

        macro_rules! next_state {
            ($finished:expr) => {
                SseStreamState {
                    body,
                    ctx,
                    decoder,
                    finished: $finished,
                    ping,
                    restarter,
                    started_at,
                }
            };
        }

        tokio::select! {
            chunk_result = body.next() => {
                match chunk_result {
                    Some(Ok(chunk)) => {
                        let mut events = Vec::new();
                        if let Err(e) = decoder.feed(&chunk) {
                            handle_stream_decode_error(&mut events, &mut ctx, &e);
                        }

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
                        Some((stream::iter(bytes), next_state!(stream_failed)))
                    }
                    Some(Err(e)) => {
                        crate::metrics::inc_stream_interrupted();

                        // 尚未向客户端输出任何事件时，断流可以透明重放：
                        // 长对话下上游常在首字节前思考数分钟，链路按空闲超时切断
                        // （hyper 报 UnexpectedEof）。此时客户端还没收到 message_start，
                        // 重放后完全无感知，比返回 error 让客户端整轮失败要好。
                        // 排除本地 720s 超时：那不是链路瞬断，重放只会再等 12 分钟
                        // 并多扣一次配额。
                        if !ctx.client_saw_output()
                            && !e.is_timeout()
                            && let Some(resp) = restarter.restart().await
                        {
                            tracing::warn!(
                                "上游断流且尚未向客户端输出，重放请求（剩余 {} 次，距流建立 {}ms）: {}",
                                restarter.remaining,
                                started_at.elapsed().as_millis(),
                                e
                            );
                            body = resp.bytes_stream();
                            decoder = EventStreamDecoder::new();
                            ctx = restarter.build_ctx();
                            let empty: Vec<Result<Bytes, Infallible>> = Vec::new();
                            return Some((stream::iter(empty), next_state!(false)));
                        }

                        // 走到这里说明透明重放不适用或已兜不住，区分三种原因便于定位根因：
                        // - saw_output：已向客户端输出正文，协议上无法重放（中途断流）
                        // - timeout：本地 720s 硬超时，故意不重放
                        // - 其余：首帧前断流但重放次数用尽 / 重放请求本身失败
                        let elapsed_ms = started_at.elapsed().as_millis();
                        ctx.stream_failed = true;
                        let events = if ctx.client_saw_output() {
                            // 中途断流：已输出正文，协议上无法重放。不向客户端发 error
                            // （那会让 Claude Code 判定整轮失败、重试整轮，多扣配额且
                            // 可能循环），而是以 stop_reason=max_tokens 优雅封口，让客户端
                            // 把已生成部分当作被截断的有效回答接着补全。
                            tracing::warn!(
                                "上游中途断流（已输出正文，无法重放，优雅收尾 max_tokens，距流建立 {}ms）: {:?}",
                                elapsed_ms,
                                e
                            );
                            ctx.finalize_stream_on_interrupt()
                        } else {
                            // 未输出正文却仍走到这里：本地超时（故意不重放），或首帧前
                            // 断流但重放次数用尽 / 重放请求本身失败。此时客户端还没收到
                            // 任何正文，发 error 让它重试整轮是正确的。
                            if e.is_timeout() {
                                tracing::error!(
                                    "读取响应流失败（本地超时，不重放，距流建立 {}ms）: {:?}",
                                    elapsed_ms,
                                    e
                                );
                            } else {
                                tracing::error!(
                                    "读取响应流失败（首帧前断流，重放未兜住，剩余 {} 次，距流建立 {}ms）: {:?}",
                                    restarter.remaining,
                                    elapsed_ms,
                                    e
                                );
                            }
                            let mut events = vec![StreamContext::create_error_event(&format!(
                                "Upstream stream error: {e}"
                            ))];
                            events.extend(ctx.finalize_stream_on_failure());
                            events
                        };
                        let bytes: Vec<Result<Bytes, Infallible>> = events
                            .into_iter()
                            .map(|ev| Ok(Bytes::from(ev.to_sse_string())))
                            .collect();
                        Some((stream::iter(bytes), next_state!(true)))
                    }
                    None => {
                        let final_events = ctx.finalize_stream();
                        let bytes: Vec<Result<Bytes, Infallible>> = final_events
                            .into_iter()
                            .map(|e| Ok(Bytes::from(e.to_sse_string())))
                            .collect();
                        Some((stream::iter(bytes), next_state!(true)))
                    }
                }
            }
            _ = ping.tick() => {
                tracing::trace!("发送 ping 保活事件");
                let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                Some((stream::iter(bytes), next_state!(false)))
            }
        }
    })
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
    passthrough_retry_after: bool,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api(request_body, Some(model)).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e, passthrough_retry_after),
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
    let mut token_usage: Option<crate::kiro::model::events::TokenUsage> = None;

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
                        Event::Metadata(metadata) => {
                            if let Some(usage) =
                                metadata.token_usage.as_ref().filter(|u| u.has_counts())
                            {
                                tracing::debug!(
                                    uncached_input_tokens = ?usage.uncached_input_tokens,
                                    output_tokens = ?usage.output_tokens,
                                    total_tokens = ?usage.total_tokens,
                                    "收到 metadataEvent，使用上游精确 token 用量"
                                );
                                token_usage = Some(usage.clone());
                            }
                        }
                        Event::InvalidState(invalid) => {
                            tracing::error!(
                                reason = %invalid.reason,
                                message = %invalid.message,
                                "收到 invalidStateEvent，上游报告会话状态非法"
                            );
                            return (
                                StatusCode::BAD_GATEWAY,
                                Json(ErrorResponse::new(
                                    "api_error",
                                    format!("Upstream reported invalid state: {invalid}"),
                                )),
                            )
                                .into_response();
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
                                // 模型输出被截断：tool_use 参数 JSON 不完整，整次调用作废。
                                // bytes 与 chars 都记，便于对照 Kiro 官方 WRITE_LIMIT（50 行）。
                                let largest = tool_json_buffers
                                    .iter()
                                    .max_by_key(|(_, buf)| buf.len())
                                    .map(|(id, buf)| (id.clone(), buf.len(), buf.chars().count()));
                                tracing::warn!(
                                    model = %model,
                                    text_bytes = text_content.len(),
                                    tool_count = tool_json_buffers.len(),
                                    largest_tool_id = ?largest.as_ref().map(|(id, _, _)| id),
                                    largest_tool_input_bytes = ?largest.as_ref().map(|(_, b, _)| b),
                                    largest_tool_input_chars = ?largest.as_ref().map(|(_, _, c)| c),
                                    upstream_message = %message,
                                    "上游内容长度超限，输出被截断（stop_reason=max_tokens）"
                                );
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

    // token 用量优先级：metadataEvent 精确值 > contextUsageEvent 反推值 > 本地估算
    let final_input_tokens = token_usage
        .as_ref()
        .and_then(|u| u.anthropic_input_tokens())
        .or(context_input_tokens)
        .unwrap_or(input_tokens);
    let final_output_tokens = token_usage
        .as_ref()
        .and_then(|u| u.anthropic_output_tokens())
        .unwrap_or_else(|| token::estimate_output_tokens(&content));

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
            "output_tokens": final_output_tokens
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
        let resp = map_provider_error(err, true);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("30")
        );
    }

    #[test]
    fn test_map_provider_error_429_omits_retry_after_when_disabled() {
        let err = anyhow::Error::new(UpstreamApiError {
            status: 429,
            message: "流式 API 请求失败: 429 Too Many Requests".to_string(),
            retry_after: Some(Duration::from_secs(30)),
        });
        let resp = map_provider_error(err, false);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_none());
    }

    #[test]
    fn test_map_provider_error_local_rpm_429_includes_retry_after() {
        let err = anyhow::Error::new(UpstreamApiError {
            status: 429,
            message: "local credential RPM limit exceeded (8/min)".to_string(),
            retry_after: Some(Duration::from_secs(12)),
        });
        let resp = map_provider_error(err, true);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
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
        let resp = map_provider_error(err, true);
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
        let resp = map_provider_error(err, true);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn test_map_provider_error_plain_error_is_502() {
        let err = anyhow::anyhow!("网络发送失败");
        let resp = map_provider_error(err, true);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn test_map_provider_error_context_full_is_400() {
        let err = anyhow::anyhow!("xxx CONTENT_LENGTH_EXCEEDS_THRESHOLD yyy");
        let resp = map_provider_error(err, true);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

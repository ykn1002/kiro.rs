//! OpenAI Chat Completions API Handler

use std::convert::Infallible;
use std::time::Duration;

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
use tokio::time::interval;
use uuid::Uuid;

use crate::anthropic::{AppState, convert_request, conversion_error_parts, get_context_window_size};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::parser::error::ParseError;
use crate::token;

use super::converter::to_anthropic_request;
use super::stream::{OpenAiChunk, OpenAiStreamContext, done_sse};
use super::types::{
    AssistantMessage, ChatCompletionChoice, ChatCompletionRequest, ChatCompletionResponse,
    ErrorResponse, FunctionCall, ResponseToolCall, Usage,
};

const PING_INTERVAL_SECS: u64 = 25;

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<AppState>,
    JsonExtractor(payload): JsonExtractor<ChatCompletionRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/chat/completions request"
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

    // OpenAI → Anthropic 格式
    let anthropic_payload = match to_anthropic_request(&payload) {
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

    // 模型名 thinking 后缀处理（与 Anthropic 端点一致）
    let mut anthropic_payload = anthropic_payload;
    override_thinking_from_model_name(&mut anthropic_payload);

    let conversion_result = match convert_request(&anthropic_payload) {
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

    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

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
    let include_usage = payload
        .stream_options
        .as_ref()
        .is_some_and(|o| o.include_usage);

    if payload.stream {
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            include_usage,
        )
        .await
    } else {
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
        )
        .await
    }
}

pub(crate) fn override_thinking_from_model_name(payload: &mut crate::anthropic::types::MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

    payload.thinking = Some(crate::anthropic::types::Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6 {
        payload.output_config = Some(crate::anthropic::types::OutputConfig {
            effort: "high".to_string(),
        });
    }
}

async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    include_usage: bool,
) -> Response {
    let response = match provider.call_api_stream(request_body, Some(model)).await {
        Ok(r) => r,
        Err(e) => return map_provider_error(e),
    };

    let ctx = OpenAiStreamContext::new(model, input_tokens, thinking_enabled, tool_name_map);
    let stream = create_openai_sse_stream(response, ctx, include_usage);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn create_openai_sse_stream(
    response: reqwest::Response,
    ctx: OpenAiStreamContext,
    include_usage: bool,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            include_usage,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, include_usage)| async move {
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

                            let mut sse_parts = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            for openai_chunk in ctx.process_kiro_event(&event) {
                                                if openai_chunk.data == serde_json::Value::String("[DONE]".to_string()) {
                                                    sse_parts.push(done_sse());
                                                } else {
                                                    sse_parts.push(openai_chunk.to_sse_string());
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        if matches!(e, ParseError::TooManyErrors { .. }) {
                                            tracing::error!("解码器停止: {}", e);
                                            ctx.stream_failed = true;
                                            crate::metrics::inc_stream_decode_failure();
                                            let err = OpenAiStreamContext::create_error_chunk(
                                                &format!("Stream decode failed: {e}"),
                                            );
                                            sse_parts.push(err.to_sse_string());
                                            sse_parts.push(done_sse());
                                        } else {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                            }

                            let bytes: Vec<Result<Bytes, Infallible>> = sse_parts
                                .into_iter()
                                .map(|s| Ok(Bytes::from(s)))
                                .collect();

                            let stream_failed = ctx.stream_failed;
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, stream_failed, ping_interval, include_usage)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {:?}", e);
                            let err = OpenAiStreamContext::create_error_chunk(&format!(
                                "Upstream stream error: {e}"
                            ));
                            let bytes = vec![
                                Ok(Bytes::from(err.to_sse_string())),
                                Ok(Bytes::from(done_sse())),
                            ];
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, include_usage)))
                        }
                        None => {
                            let final_chunks = if ctx.stream_failed {
                                vec![OpenAiChunk {
                                    data: serde_json::Value::String("[DONE]".to_string()),
                                }]
                            } else {
                                ctx.generate_final_chunks(include_usage)
                            };
                            let bytes: Vec<Result<Bytes, Infallible>> = final_chunks
                                .into_iter()
                                .map(|c| {
                                    if c.data == serde_json::Value::String("[DONE]".to_string()) {
                                        Ok(Bytes::from(done_sse()))
                                    } else {
                                        Ok(Bytes::from(c.to_sse_string()))
                                    }
                                })
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, include_usage)))
                        }
                    }
                }
                _ = ping_interval.tick() => {
                    // OpenAI 客户端通常不需要 ping，跳过以保持兼容
                    Some((stream::iter(Vec::<Result<Bytes, Infallible>>::new()), (body_stream, ctx, decoder, false, ping_interval, include_usage)))
                }
            }
        },
    )
    .flatten()
}

async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    let response = match provider.call_api(request_body, Some(model)).await {
        Ok(r) => r,
        Err(e) => return map_provider_error(e),
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
    let mut tool_json_buffers: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut finish_reason = "stop".to_string();
    let mut prompt_tokens = input_tokens;

    for result in decoder.decode_iter() {
        if let Ok(frame) = result {
            if let Ok(event) = Event::from_frame(frame) {
                match event {
                    Event::AssistantResponse(resp) => text_content.push_str(&resp.content),
                    Event::ToolUse(tool_use) => {
                        let entry = tool_json_buffers
                            .entry(tool_use.tool_use_id.clone())
                            .or_insert_with(|| {
                                let name = tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());
                                (name, String::new())
                            });
                        entry.1.push_str(&tool_use.input);
                    }
                    Event::ContextUsage(cu) => {
                        let window = get_context_window_size(model);
                        prompt_tokens =
                            (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                        if cu.context_usage_percentage >= 100.0 {
                            finish_reason = "length".to_string();
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
                            finish_reason = "length".to_string();
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

    let mut reasoning_content: Option<String> = None;
    let mut content_text = text_content.clone();

    if thinking_enabled {
        let (thinking, remaining) =
            crate::anthropic::extract_thinking_from_complete_text(&text_content);
        reasoning_content = thinking;
        content_text = remaining;
    } else if content_text.contains("<thinking>") {
        // 未启用 thinking 时仍剥离标签，避免泄露给客户端
        let (_, remaining) =
            crate::anthropic::extract_thinking_from_complete_text(&text_content);
        content_text = remaining;
    }

    let tool_calls: Vec<ResponseToolCall> = tool_json_buffers
        .into_iter()
        .map(|(id, (name, args))| ResponseToolCall {
            id,
            call_type: "function".to_string(),
            function: FunctionCall { name, arguments: args },
        })
        .collect();

    if !tool_calls.is_empty() && finish_reason == "stop" {
        finish_reason = "tool_calls".to_string();
    }

    let content = if content_text.is_empty() {
        None
    } else {
        Some(content_text)
    };

    let completion_tokens = token::estimate_output_tokens(&[serde_json::json!({
        "type": "text",
        "text": text_content
    })]);

    let message = AssistantMessage {
        role: "assistant".to_string(),
        content,
        reasoning_content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
    };

    let resp = ChatCompletionResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: model.to_string(),
        choices: vec![ChatCompletionChoice {
            index: 0,
            message,
            finish_reason: Some(finish_reason),
            logprobs: None,
        }],
        usage: Usage {
            prompt_tokens,
            completion_tokens: completion_tokens.max(1),
            total_tokens: prompt_tokens + completion_tokens.max(1),
        },
    };

    (StatusCode::OK, Json(resp)).into_response()
}

pub(crate) fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();

    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    if err_str.contains("Input is too long") {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }

    if let Some(api_err) = err.downcast_ref::<crate::kiro::provider::UpstreamApiError>() {
        match api_err.status {
            429 => {
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
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(ErrorResponse::new(
                        "insufficient_quota",
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
            "server_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

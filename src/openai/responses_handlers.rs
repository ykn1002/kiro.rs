//! OpenAI Responses API Handler

use std::convert::Infallible;

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
use uuid::Uuid;

use crate::anthropic::{AppState, ConversionError, get_context_window_size};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;

use super::handlers::{map_provider_error, override_thinking_from_model_name};
use super::responses_converter::responses_to_anthropic;
use super::responses_stream::ResponsesStreamContext;
use super::responses_types::ResponsesRequest;
use super::types::ErrorResponse;

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
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
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
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
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

    // Codex 恒为 stream=true；未指定时也走流式
    if payload.stream {
        handle_responses_stream(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
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
        )
        .await
    }
}

async fn handle_responses_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    let response = match provider.call_api_stream(request_body).await {
        Ok(r) => r,
        Err(e) => return map_provider_error(e),
    };

    let mut ctx = ResponsesStreamContext::new(model, input_tokens, thinking_enabled, tool_name_map);
    let initial = ctx.generate_initial_events();
    let stream = create_responses_sse_stream(response, ctx, initial);

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
    ctx: ResponsesStreamContext,
    initial_events: Vec<super::responses_stream::ResponsesSseEvent>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false),
        |(mut body_stream, mut ctx, mut decoder, finished)| async move {
            if finished {
                return None;
            }

            match body_stream.next().await {
                Some(Ok(chunk)) => {
                    if let Err(e) = decoder.feed(&chunk) {
                        tracing::warn!("缓冲区溢出: {}", e);
                    }

                    let mut sse_parts = Vec::new();
                    for result in decoder.decode_iter() {
                        if let Ok(frame) = result {
                            if let Ok(event) = Event::from_frame(frame) {
                                for ev in ctx.process_kiro_event(&event) {
                                    sse_parts.push(ev.to_sse_string());
                                }
                            }
                        }
                    }

                    let bytes: Vec<Result<Bytes, Infallible>> = sse_parts
                        .into_iter()
                        .map(|s| Ok(Bytes::from(s)))
                        .collect();

                            let stream_failed = ctx.stream_failed;
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, stream_failed)))
                }
                Some(Err(e)) => {
                    tracing::error!("读取响应流失败: {:?}", e);
                    let err = super::responses_stream::ResponsesStreamContext::create_error_event(
                        &format!("Upstream stream error: {e}"),
                    );
                    let bytes = vec![Ok(Bytes::from(err.to_sse_string()))];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, true)))
                }
                None => {
                    let final_events = if ctx.stream_failed {
                        Vec::new()
                    } else {
                        ctx.generate_final_events()
                    };
                    let bytes: Vec<Result<Bytes, Infallible>> = final_events
                        .into_iter()
                        .map(|e| Ok(Bytes::from(e.to_sse_string())))
                        .collect();
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, true)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

async fn handle_responses_non_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    let response = match provider.call_api(request_body).await {
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
                    Event::Exception { exception_type, .. } => {
                        if exception_type == "ContentLengthExceededException" {
                            status = "incomplete".to_string();
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
        let (_, remaining) =
            crate::anthropic::extract_thinking_from_complete_text(&text_content);
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

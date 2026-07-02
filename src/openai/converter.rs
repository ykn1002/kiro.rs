//! OpenAI Chat Completions → Anthropic Messages 协议转换

use std::collections::HashMap;

use crate::anthropic::{ConversionError, map_model, metadata_from_openai_extra};
use crate::anthropic::types::{Message, MessagesRequest, SystemMessage, Thinking, Tool};

use super::types::ChatCompletionRequest;

/// 将 OpenAI Chat Completions 请求转换为 Anthropic MessagesRequest
pub fn to_anthropic_request(req: &ChatCompletionRequest) -> Result<MessagesRequest, ConversionError> {
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 校验模型
    let _ = map_model(&req.model).ok_or_else(|| ConversionError::UnsupportedModel(req.model.clone()))?;

    let max_tokens = req
        .max_completion_tokens
        .or(req.max_tokens)
        .unwrap_or(8192);

    let (system, messages) = convert_messages(&req.messages)?;

    let tools = convert_tools(&req.tools);
    let tool_choice = convert_tool_choice(&req.tool_choice);
    let (thinking, output_config) = infer_thinking_config(req);

    Ok(MessagesRequest {
        model: req.model.clone(),
        max_tokens,
        messages,
        stream: req.stream,
        system,
        tools,
        tool_choice,
        thinking,
        output_config,
        metadata: metadata_from_openai_extra(&req.extra),
    })
}

fn convert_messages(
    messages: &[super::types::ChatMessage],
) -> Result<(Option<Vec<SystemMessage>>, Vec<Message>), ConversionError> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut anthropic_messages: Vec<Message> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" | "developer" => {
                if let Some(text) = extract_text_content(msg.content.as_ref()) {
                    if !text.is_empty() {
                        system_parts.push(text);
                    }
                }
            }
            "user" => {
                anthropic_messages.push(Message {
                    role: "user".to_string(),
                    content: convert_user_content(msg),
                });
            }
            "assistant" => {
                anthropic_messages.push(convert_assistant_message(msg));
            }
            "tool" => {
                let tool_call_id = msg
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                let result_text = extract_text_content(msg.content.as_ref()).unwrap_or_default();
                anthropic_messages.push(Message {
                    role: "user".to_string(),
                    content: serde_json::json!([{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": result_text
                    }]),
                });
            }
            other => {
                tracing::warn!("未知消息角色 \"{}\"，按 user 处理", other);
                anthropic_messages.push(Message {
                    role: "user".to_string(),
                    content: convert_user_content(msg),
                });
            }
        }
    }

    if anthropic_messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(vec![SystemMessage {
            text: system_parts.join("\n\n"),
            cache_control: None,
        }])
    };

    Ok((system, anthropic_messages))
}

fn convert_user_content(msg: &super::types::ChatMessage) -> serde_json::Value {
    let Some(content) = &msg.content else {
        return serde_json::Value::String(String::new());
    };

    match content {
        serde_json::Value::String(s) => serde_json::Value::String(s.clone()),
        serde_json::Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                let Some(part_type) = part.get("type").and_then(|v| v.as_str()) else {
                    continue;
                };
                match part_type {
                    "text" => {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            blocks.push(serde_json::json!({
                                "type": "text",
                                "text": text
                            }));
                        }
                    }
                    "image_url" => {
                        if let Some(url) = part
                            .get("image_url")
                            .and_then(|v| v.get("url"))
                            .and_then(|v| v.as_str())
                        {
                            if let Some((media_type, data)) = parse_data_url(url) {
                                blocks.push(serde_json::json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": media_type,
                                        "data": data
                                    }
                                }));
                            }
                        }
                    }
                    _ => {}
                }
            }
            if blocks.is_empty() {
                serde_json::Value::String(String::new())
            } else {
                serde_json::Value::Array(blocks)
            }
        }
        other => other.clone(),
    }
}

fn convert_assistant_message(msg: &super::types::ChatMessage) -> Message {
    let mut blocks: Vec<serde_json::Value> = Vec::new();

    if let Some(text) = extract_text_content(msg.content.as_ref()) {
        if !text.is_empty() {
            blocks.push(serde_json::json!({
                "type": "text",
                "text": text
            }));
        }
    }

    if let Some(tool_calls) = &msg.tool_calls {
        for tc in tool_calls {
            let input: serde_json::Value = if tc.function.arguments.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}))
            };
            blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.function.name,
                "input": input
            }));
        }
    }

    let content = if blocks.is_empty() {
        serde_json::Value::String(String::new())
    } else if blocks.len() == 1 && blocks[0].get("type").and_then(|v| v.as_str()) == Some("text") {
        serde_json::Value::String(
            blocks[0]
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        )
    } else {
        serde_json::Value::Array(blocks)
    };

    Message {
        role: "assistant".to_string(),
        content,
    }
}

fn extract_text_content(content: Option<&serde_json::Value>) -> Option<String> {
    let content = content?;
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            let mut texts = Vec::new();
            for part in parts {
                if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        texts.push(text.to_string());
                    }
                }
            }
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(",")?;
    let media_type = meta.split(';').next()?.to_string();
    Some((media_type, data.to_string()))
}

fn convert_tools(tools: &Option<Vec<super::types::ChatTool>>) -> Option<Vec<Tool>> {
    let tools = tools.as_ref()?;
    if tools.is_empty() {
        return None;
    }

    Some(
        tools
            .iter()
            .filter(|t| t.tool_type == "function")
            .map(|t| {
                let params = if t.function.parameters.is_null() {
                    HashMap::new()
                } else if let serde_json::Value::Object(obj) = &t.function.parameters {
                    obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                } else {
                    HashMap::new()
                };
                Tool {
                    tool_type: None,
                    name: t.function.name.clone(),
                    description: t.function.description.clone(),
                    input_schema: params,
                    max_uses: None,
                }
            })
            .collect(),
    )
}

fn convert_tool_choice(tool_choice: &Option<serde_json::Value>) -> Option<serde_json::Value> {
    let Some(choice) = tool_choice else {
        return None;
    };

    match choice {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(serde_json::json!({"type": "auto"})),
            "none" => Some(serde_json::json!({"type": "none"})),
            "required" => Some(serde_json::json!({"type": "any"})),
            _ => Some(choice.clone()),
        },
        serde_json::Value::Object(obj) => {
            if obj.get("type").and_then(|v| v.as_str()) == Some("function") {
                if let Some(name) = obj
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                {
                    return Some(serde_json::json!({"type": "tool", "name": name}));
                }
            }
            Some(choice.clone())
        }
        _ => Some(choice.clone()),
    }
}

fn infer_thinking_config(
    req: &ChatCompletionRequest,
) -> (Option<Thinking>, Option<crate::anthropic::types::OutputConfig>) {
    let model_lower = req.model.to_lowercase();
    let has_thinking_suffix = model_lower.contains("thinking");
    let has_reasoning_effort = req
        .reasoning_effort
        .as_ref()
        .is_some_and(|e| !e.is_empty() && e != "none");

    if !has_thinking_suffix && !has_reasoning_effort {
        return (None, None);
    }

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

    let thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    let output_config = if is_opus_4_6 {
        Some(crate::anthropic::types::OutputConfig {
            effort: req
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "high".to_string()),
        })
    } else {
        None
    };

    (thinking, output_config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::init_model_registry;
    use crate::model::config::default_models;

    fn setup() {
        let _ = init_model_registry(default_models());
    }

    #[test]
    fn test_basic_user_message() {
        setup();
        let req = ChatCompletionRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![super::super::types::ChatMessage {
                role: "user".to_string(),
                content: Some(serde_json::json!("hello")),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            stream: false,
            max_tokens: Some(1024),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            reasoning_effort: None,
            stream_options: None,
            n: None,
            stop: None,
            extra: HashMap::new(),
        };

        let anthropic = to_anthropic_request(&req).unwrap();
        assert_eq!(anthropic.messages.len(), 1);
        assert_eq!(anthropic.max_tokens, 1024);
    }

    #[test]
    fn test_system_and_tool_messages() {
        setup();
        let req = ChatCompletionRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![
                super::super::types::ChatMessage {
                    role: "system".to_string(),
                    content: Some(serde_json::json!("You are helpful")),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
                super::super::types::ChatMessage {
                    role: "user".to_string(),
                    content: Some(serde_json::json!("hi")),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
                super::super::types::ChatMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: Some(vec![super::super::types::ToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: super::super::types::FunctionCall {
                            name: "get_weather".to_string(),
                            arguments: r#"{"city":"Beijing"}"#.to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                },
                super::super::types::ChatMessage {
                    role: "tool".to_string(),
                    content: Some(serde_json::json!("sunny")),
                    tool_calls: None,
                    tool_call_id: Some("call_1".to_string()),
                    name: None,
                },
            ],
            stream: false,
            max_tokens: None,
            max_completion_tokens: Some(2048),
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            reasoning_effort: None,
            stream_options: None,
            n: None,
            stop: None,
            extra: HashMap::new(),
        };

        let anthropic = to_anthropic_request(&req).unwrap();
        assert_eq!(anthropic.system.as_ref().unwrap()[0].text, "You are helpful");
        assert_eq!(anthropic.max_tokens, 2048);
        assert_eq!(anthropic.messages.len(), 3);
    }

    #[test]
    fn test_thinking_from_model_suffix() {
        setup();
        let req = ChatCompletionRequest {
            model: "claude-sonnet-4-6-thinking".to_string(),
            messages: vec![super::super::types::ChatMessage {
                role: "user".to_string(),
                content: Some(serde_json::json!("think")),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            stream: true,
            max_tokens: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            reasoning_effort: None,
            stream_options: None,
            n: None,
            stop: None,
            extra: HashMap::new(),
        };

        let anthropic = to_anthropic_request(&req).unwrap();
        assert!(anthropic.thinking.as_ref().unwrap().is_enabled());
    }
}

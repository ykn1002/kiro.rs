//! OpenAI Responses API → Anthropic Messages 协议转换

use std::collections::HashMap;

use crate::anthropic::{ConversionError, map_model};
use crate::anthropic::types::{Message, MessagesRequest, SystemMessage, Thinking, Tool};

use super::responses_types::ResponsesRequest;

/// 将 Responses API 请求转换为 Anthropic MessagesRequest
pub fn responses_to_anthropic(req: &ResponsesRequest) -> Result<MessagesRequest, ConversionError> {
    let input = apply_compaction(&req.input);

    if input.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    let _ = map_model(&req.model).ok_or_else(|| ConversionError::UnsupportedModel(req.model.clone()))?;

    let max_tokens = req.max_output_tokens.unwrap_or(8192);

    let mut system_parts: Vec<String> = Vec::new();
    if let Some(instructions) = &req.instructions {
        if !instructions.is_empty() {
            system_parts.push(instructions.clone());
        }
    }

    let mut messages: Vec<Message> = Vec::new();

    for item in &input {
        if let Some(parsed) = parse_responses_input_item(item) {
            match parsed {
                ParsedInputItem::System(text) => {
                    if !text.is_empty() {
                        system_parts.push(text);
                    }
                }
                ParsedInputItem::Message(msg) => messages.push(msg),
            }
        }
    }

    normalize_messages(&mut messages);

    if messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    if let Some(last) = messages.last() {
        if last.role == "user" && message_content_is_empty(&last.content) {
            tracing::warn!(
                "Responses 转换后末尾 user 消息内容为空，input_items={}",
                input.len()
            );
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(vec![SystemMessage {
            text: system_parts.join("\n\n"),
        }])
    };

    let tools = convert_response_tools(&req.tools);
    let tool_choice = convert_response_tool_choice(&req.tool_choice);
    let (thinking, output_config) = infer_thinking_from_responses(req);

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
        metadata: None,
    })
}

enum ParsedInputItem {
    System(String),
    Message(Message),
}

/// 解析单条 Responses input item
fn parse_responses_input_item(item: &serde_json::Value) -> Option<ParsedInputItem> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match item_type {
        "function_call" | "custom_tool_call" | "local_shell_call" => {
            return Some(ParsedInputItem::Message(convert_function_call_item(item)));
        }
        "function_call_output" | "custom_tool_call_output" => {
            return Some(ParsedInputItem::Message(convert_function_output_item(item)));
        }
        "agent_message" => {
            return Some(ParsedInputItem::Message(convert_agent_message_item(item)));
        }
        "reasoning"
        | "compaction"
        | "compaction_summary"
        | "context_compaction"
        | "compaction_trigger"
        | "additional_tools"
        | "web_search_call"
        | "image_generation_call"
        | "tool_search_call"
        | "tool_search_output" => return None,
        "message" => {}
        other if !other.is_empty() => {
            tracing::debug!("跳过 Responses input item type: {}", other);
            return None;
        }
        _ => {}
    }

    item.get("role")
        .and_then(|v| v.as_str())
        .map(|role| parse_role_message(role, item))
}

fn parse_role_message(role: &str, item: &serde_json::Value) -> ParsedInputItem {
    match role {
        "developer" | "system" => ParsedInputItem::System(
            extract_response_content_text(item.get("content")).unwrap_or_default(),
        ),
        "assistant" => ParsedInputItem::Message(convert_response_assistant(item)),
        "user" | "tool" => ParsedInputItem::Message(Message {
            role: "user".to_string(),
            content: convert_response_content(item.get("content")),
        }),
        other => {
            tracing::warn!("Responses input 未知 role \"{}\"，按 user 处理", other);
            ParsedInputItem::Message(Message {
                role: "user".to_string(),
                content: convert_response_content(item.get("content")),
            })
        }
    }
}

fn normalize_messages(messages: &mut Vec<Message>) {
    while messages.last().is_some_and(|m| m.role == "user" && message_content_is_empty(&m.content))
    {
        messages.pop();
    }

    while messages.last().is_some_and(|m| {
        m.role == "assistant"
            && message_content_is_empty(&m.content)
            && !assistant_has_tool_use(&m.content)
    }) {
        messages.pop();
    }
}

fn assistant_has_tool_use(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::Array(arr) => arr
            .iter()
            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_use")),
        _ => false,
    }
}

fn message_content_is_empty(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::String(s) => s.trim().is_empty(),
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return true;
            }
            arr.iter().all(|part| {
                if matches!(
                    part.get("type").and_then(|t| t.as_str()),
                    Some("tool_use") | Some("tool_result") | Some("image")
                ) {
                    return false;
                }
                part_text_is_empty(part)
            })
        }
        serde_json::Value::Object(obj) => obj
            .get("text")
            .and_then(|v| v.as_str())
            .is_none_or(|t| t.trim().is_empty()),
        serde_json::Value::Null => true,
        _ => false,
    }
}

fn part_text_is_empty(part: &serde_json::Value) -> bool {
    response_content_part_to_text(part)
        .is_none_or(|t| t.trim().is_empty())
}

fn is_compaction_item(item: &serde_json::Value) -> bool {
    matches!(
        item.get("type").and_then(|t| t.as_str()),
        Some("compaction") | Some("compaction_summary") | Some("context_compaction")
    )
}

/// 裁剪 compaction 标记之前的历史
fn apply_compaction(input: &[serde_json::Value]) -> Vec<serde_json::Value> {
    if let Some(idx) = input.iter().rposition(is_compaction_item) {
        let after = &input[idx + 1..];
        if after.is_empty() {
            // compaction 在末尾且无后续 item：忽略该标记，保留其余历史
            input
                .iter()
                .filter(|item| !is_compaction_item(item))
                .cloned()
                .collect()
        } else {
            after.to_vec()
        }
    } else {
        input.to_vec()
    }
}

fn response_content_part_to_text(part: &serde_json::Value) -> Option<String> {
    match part {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Object(map) => map
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    }
}

fn extract_response_content_text(content: Option<&serde_json::Value>) -> Option<String> {
    let content = content?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(response_content_part_to_text)
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(_) => response_content_part_to_text(content).unwrap_or_default(),
        _ => return None,
    };
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn convert_response_content(content: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(content) = content else {
        return serde_json::Value::String(String::new());
    };

    match content {
        serde_json::Value::String(s) => serde_json::Value::String(s.clone()),
        serde_json::Value::Object(part) => {
            if let Some(text) = response_content_part_to_text(&serde_json::Value::Object(part.clone()))
            {
                if !text.is_empty() {
                    return serde_json::Value::String(text);
                }
            }
            convert_response_content_array(&[serde_json::Value::Object(part.clone())])
        }
        serde_json::Value::Array(parts) => convert_response_content_array(parts),
        other => other.clone(),
    }
}

fn convert_response_content_array(parts: &[serde_json::Value]) -> serde_json::Value {
    let mut blocks = Vec::new();
    for part in parts {
        let typ = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(text) = response_content_part_to_text(part) {
            if !text.is_empty() {
                blocks.push(serde_json::json!({"type": "text", "text": text}));
                continue;
            }
        }
        match typ {
            "input_image" => {
                let url = part
                    .get("image_url")
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .or_else(|| v.get("url").and_then(|u| u.as_str()).map(str::to_string))
                    });
                if let Some(url) = url {
                    if let Some((media_type, data)) = parse_data_url(&url) {
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
            "input_file" => {
                if let Some(text) = part
                    .get("file_data")
                    .or_else(|| part.get("data"))
                    .and_then(|v| v.as_str())
                {
                    blocks.push(serde_json::json!({"type": "text", "text": text}));
                }
            }
            _ => {}
        }
    }
    if blocks.is_empty() {
        serde_json::Value::String(String::new())
    } else if blocks.len() == 1 {
        let block = blocks.into_iter().next().unwrap();
        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
            serde_json::Value::String(
                block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        } else {
            block
        }
    } else {
        serde_json::Value::Array(blocks)
    }
}

fn convert_agent_message_item(item: &serde_json::Value) -> Message {
    let text = extract_response_content_text(item.get("content")).unwrap_or_default();
    Message {
        role: "user".to_string(),
        content: serde_json::Value::String(text),
    }
}

fn convert_response_assistant(item: &serde_json::Value) -> Message {
    let mut blocks: Vec<serde_json::Value> = Vec::new();

    if let Some(text) = extract_response_content_text(item.get("content")) {
        if !text.is_empty() {
            blocks.push(serde_json::json!({"type": "text", "text": text}));
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

fn convert_function_call_item(item: &serde_json::Value) -> Message {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let name = item
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let arguments = item
        .get("arguments")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("input").and_then(|v| v.as_str()))
        .unwrap_or("{}");
    let input: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));

    Message {
        role: "assistant".to_string(),
        content: serde_json::json!([{
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": input
        }]),
    }
}

fn convert_function_output_item(item: &serde_json::Value) -> Message {
    let call_id = item
        .get("call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let output = item.get("output").map(extract_output_value).unwrap_or_default();

    Message {
        role: "user".to_string(),
        content: serde_json::json!([{
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": output
        }]),
    }
}

fn extract_output_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.split(';').next()?.to_string();
    Some((media_type, data.to_string()))
}

fn convert_response_tools(tools: &Option<Vec<serde_json::Value>>) -> Option<Vec<Tool>> {
    let tools = tools.as_ref()?;
    let mut result = Vec::new();

    for tool in tools {
        let typ = tool.get("type").and_then(|v| v.as_str()).unwrap_or("function");
        match typ {
            "function" => {
                let name = tool
                    .get("name")
                    .or_else(|| tool.get("function").and_then(|f| f.get("name")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let description = tool
                    .get("description")
                    .or_else(|| tool.get("function").and_then(|f| f.get("description")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let params = tool
                    .get("parameters")
                    .or_else(|| tool.get("function").and_then(|f| f.get("parameters")))
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let input_schema = if let serde_json::Value::Object(obj) = params {
                    obj.into_iter().collect()
                } else {
                    HashMap::new()
                };
                result.push(Tool {
                    tool_type: None,
                    name,
                    description,
                    input_schema,
                    max_uses: None,
                });
            }
            "custom" | "apply_patch" => {
                // Codex apply_patch：转为普通 function 工具
                let name = tool
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("apply_patch")
                    .to_string();
                let description = tool
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Apply a patch to files")
                    .to_string();
                result.push(Tool {
                    tool_type: None,
                    name,
                    description,
                    input_schema: HashMap::from([
                        (
                            "type".to_string(),
                            serde_json::Value::String("object".to_string()),
                        ),
                        ("properties".to_string(), serde_json::json!({})),
                        (
                            "required".to_string(),
                            serde_json::Value::Array(Vec::new()),
                        ),
                    ]),
                    max_uses: None,
                });
            }
            _ => tracing::debug!("跳过未知 Responses tool type: {}", typ),
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn convert_response_tool_choice(tool_choice: &Option<serde_json::Value>) -> Option<serde_json::Value> {
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
        _ => Some(choice.clone()),
    }
}

fn infer_thinking_from_responses(
    req: &ResponsesRequest,
) -> (Option<Thinking>, Option<crate::anthropic::types::OutputConfig>) {
    let model_lower = req.model.to_lowercase();
    let has_thinking_suffix = model_lower.contains("thinking");

    let reasoning_effort = req
        .reasoning
        .as_ref()
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str())
        .filter(|e| !e.is_empty() && *e != "none");

    if !has_thinking_suffix && reasoning_effort.is_none() {
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
            effort: reasoning_effort.unwrap_or("high").to_string(),
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
    fn test_responses_basic() {
        setup();
        let req = ResponsesRequest {
            model: "claude-sonnet-4-6".to_string(),
            instructions: Some("You are helpful".to_string()),
            input: vec![serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            })],
            tools: None,
            tool_choice: None,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            extra: HashMap::new(),
        };
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(anthropic.messages.len(), 1);
        assert!(anthropic.system.is_some());
    }

    #[test]
    fn test_compaction_trims_history() {
        setup();
        let req = ResponsesRequest {
            model: "claude-sonnet-4-6".to_string(),
            instructions: None,
            input: vec![
                serde_json::json!({"role": "user", "content": [{"type": "input_text", "text": "old"}]}),
                serde_json::json!({"type": "compaction"}),
                serde_json::json!({"role": "user", "content": [{"type": "input_text", "text": "new"}]}),
            ],
            tools: None,
            tool_choice: None,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            extra: HashMap::new(),
        };
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(anthropic.messages.len(), 1);
    }

    #[test]
    fn test_codex_message_format() {
        setup();
        let req = ResponsesRequest {
            model: "claude-sonnet-4-6".to_string(),
            instructions: Some("You are Codex".to_string()),
            input: vec![serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "你是谁"}]
            })],
            tools: None,
            tool_choice: None,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            extra: HashMap::new(),
        };
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(anthropic.messages.len(), 1);
        assert_eq!(
            anthropic.messages[0].content.as_str().unwrap(),
            "你是谁"
        );
    }

    #[test]
    fn test_content_single_object() {
        setup();
        let req = ResponsesRequest {
            model: "claude-sonnet-4-6".to_string(),
            instructions: None,
            input: vec![serde_json::json!({
                "type": "message",
                "role": "user",
                "content": {"type": "input_text", "text": "hello object"}
            })],
            tools: None,
            tool_choice: None,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            extra: HashMap::new(),
        };
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(
            anthropic.messages[0].content.as_str().unwrap(),
            "hello object"
        );
    }

    #[test]
    fn test_trailing_empty_user_stripped() {
        setup();
        let req = ResponsesRequest {
            model: "claude-sonnet-4-6".to_string(),
            instructions: None,
            input: vec![
                serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "真实问题"}]
                }),
                serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": ""}]
                }),
            ],
            tools: None,
            tool_choice: None,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            extra: HashMap::new(),
        };
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(anthropic.messages.len(), 1);
        assert_eq!(
            anthropic.messages[0].content.as_str().unwrap(),
            "真实问题"
        );
    }

    #[test]
    fn test_deserialize_string_input() {
        setup();
        let json = r#"{"model":"claude-sonnet-4-6","input":"你好","stream":true}"#;
        let req: super::super::responses_types::ResponsesRequest =
            serde_json::from_str(json).unwrap();
        assert_eq!(req.input.len(), 1);
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(anthropic.messages[0].content.as_str().unwrap(), "你好");
    }

    #[test]
    fn test_assistant_output_text_in_history() {
        setup();
        let req = ResponsesRequest {
            model: "claude-sonnet-4-6".to_string(),
            instructions: None,
            input: vec![
                serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}]
                }),
                serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello back"}]
                }),
                serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "again"}]
                }),
            ],
            tools: None,
            tool_choice: None,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            extra: HashMap::new(),
        };
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(anthropic.messages.len(), 3);
    }

    #[test]
    fn test_compaction_at_end_keeps_history() {
        setup();
        let req = ResponsesRequest {
            model: "claude-sonnet-4-6".to_string(),
            instructions: None,
            input: vec![
                serde_json::json!({"role": "user", "content": [{"type": "input_text", "text": "keep me"}]}),
                serde_json::json!({"type": "compaction", "encrypted_content": "x"}),
            ],
            tools: None,
            tool_choice: None,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            extra: HashMap::new(),
        };
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(anthropic.messages.len(), 1);
        assert_eq!(anthropic.messages[0].content.as_str().unwrap(), "keep me");
    }

    #[test]
    fn test_function_call_items() {
        setup();
        let req = ResponsesRequest {
            model: "claude-sonnet-4-6".to_string(),
            instructions: None,
            input: vec![
                serde_json::json!({"role": "user", "content": [{"type": "input_text", "text": "run"}]}),
                serde_json::json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "shell",
                    "arguments": r#"{"command":"ls"}"#
                }),
                serde_json::json!({
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "file.txt"
                }),
            ],
            tools: None,
            tool_choice: None,
            stream: true,
            max_output_tokens: None,
            reasoning: None,
            extra: HashMap::new(),
        };
        let anthropic = responses_to_anthropic(&req).unwrap();
        assert_eq!(anthropic.messages.len(), 3);
    }
}

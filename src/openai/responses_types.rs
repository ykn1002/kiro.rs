//! OpenAI Responses API 类型定义

use serde::Deserialize;
use std::collections::HashMap;

/// Responses API 请求体（Codex 使用 `wire_api = "responses"`）
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub instructions: Option<String>,
    /// Codex / OpenAI 允许 `input` 为字符串、单条对象或数组
    #[serde(default, deserialize_with = "deserialize_responses_input")]
    pub input: Vec<serde_json::Value>,
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_output_tokens: Option<i32>,
    #[serde(default)]
    pub reasoning: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// 将 Responses `input` 统一规范为 item 数组
fn deserialize_responses_input<'de, D>(deserializer: D) -> Result<Vec<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct InputVisitor;

    impl<'de> Visitor<'de> for InputVisitor {
        type Value = Vec<serde_json::Value>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string, object, or array of Responses input items")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(vec![serde_json::json!({
                "type": "message",
                "role": "user",
                "content": value
            })])
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element()? {
                items.push(item);
            }
            Ok(items)
        }

        fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let item = serde_json::Map::<String, serde_json::Value>::deserialize(
                de::value::MapAccessDeserializer::new(map),
            )?;
            Ok(vec![serde_json::Value::Object(item)])
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Vec::new())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(InputVisitor)
}

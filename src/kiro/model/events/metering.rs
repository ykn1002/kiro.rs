//! 计费事件
//!
//! 处理 meteringEvent 类型的事件。上游在一次响应结束时下发本次请求消耗的
//! credits（Kiro 的实际计费单位），`usage` 为消耗量（可为 0.25 等分数），
//! `unit`/`unit_plural` 为单位名（如 `credit`/`credits`）。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 计费事件
///
/// 包含本次请求的资源使用计费信息（credits 消耗）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    /// 计费单位（单数形式），如 `credit`
    #[serde(default)]
    pub unit: String,
    /// 计费单位（复数形式），如 `credits`
    #[serde(default)]
    pub unit_plural: String,
    /// 本次请求消耗量（credits，可为分数）
    #[serde(default)]
    pub usage: f64,
}

impl EventPayload for MeteringEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_metering_payload() {
        let e: MeteringEvent =
            serde_json::from_str(r#"{"unit":"credit","unitPlural":"credits","usage":0.25}"#)
                .unwrap();
        assert_eq!(e.unit, "credit");
        assert_eq!(e.unit_plural, "credits");
        assert_eq!(e.usage, 0.25);
    }

    #[test]
    fn test_parse_metering_missing_fields() {
        // 上游字段缺失或新增都不应导致解析失败
        let e: MeteringEvent =
            serde_json::from_str(r#"{"usage":1.0,"somethingNew":true}"#).unwrap();
        assert_eq!(e.usage, 1.0);
        assert!(e.unit.is_empty());
    }
}

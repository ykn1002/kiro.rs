//! 透传响应的轻量 usage 统计
//!
//! 透传上游返回标准协议 SSE（Anthropic / OpenAI）。这里旁路 sniff 出 usage
//! 字段，在响应流结束时落库（`stats_db` + `model_stats`）。sniff 不到（断流、
//! 格式异常等）则仅记一次请求计数，绝不阻塞或缓冲整条流。
//!
//! 策略：维护一个有限大小的滚动尾缓冲（usage 事件通常出现在流末尾），
//! 结束时从尾缓冲中提取最后一次出现的 usage。

use bytes::Bytes;

/// 尾缓冲最大字节数（usage 事件位于流末尾，无需保留全部响应）
const TAIL_BUFFER_MAX: usize = 64 * 1024;

/// 透传响应协议（决定 usage 的字段形态）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassthroughKind {
    /// Anthropic：`message_start.usage.input_tokens` + `message_delta.usage.output_tokens`
    Anthropic,
    /// OpenAI Responses：`response.completed.response.usage`
    Openai,
}

/// 从透传响应流中 sniff usage，drop 时落库
pub struct PassthroughUsageSniffer {
    model: String,
    credential_id: u64,
    kind: PassthroughKind,
    /// 滚动尾缓冲（UTF-8 文本，超出上限时从头部丢弃）
    tail: String,
    /// 是否已 finalize（避免 Drop 重复落库）
    finalized: bool,
}

impl PassthroughUsageSniffer {
    pub fn new(model: String, credential_id: u64, kind: PassthroughKind) -> Self {
        Self {
            model,
            credential_id,
            kind,
            tail: String::new(),
            finalized: false,
        }
    }

    /// 喂入一段响应字节（UTF-8 非法片段按 lossy 处理，仅用于 sniff）
    pub fn feed(&mut self, chunk: &Bytes) {
        // 仅用于字段搜索，lossy 解码可接受
        self.tail.push_str(&String::from_utf8_lossy(chunk));
        if self.tail.len() > TAIL_BUFFER_MAX {
            // 从字符边界安全地截断前部
            let cut = self.tail.len() - TAIL_BUFFER_MAX;
            let mut idx = cut;
            while idx < self.tail.len() && !self.tail.is_char_boundary(idx) {
                idx += 1;
            }
            self.tail.drain(..idx);
        }
    }

    /// 提取 usage 并落库；无论 sniff 成功与否都记一次请求。
    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;

        let (input_tokens, output_tokens, cache_read, cache_write) = match self.kind {
            PassthroughKind::Anthropic => sniff_anthropic_usage(&self.tail),
            PassthroughKind::Openai => sniff_openai_usage(&self.tail),
        }
        .unwrap_or((0, 0, 0, 0));

        crate::model_stats::record(&self.model, input_tokens, output_tokens, 0.0);
        crate::stats_db::record(
            &self.model,
            self.credential_id as i64,
            input_tokens,
            output_tokens,
            cache_read,
            cache_write,
            0.0,
        );

        tracing::debug!(
            model = %self.model,
            credential_id = self.credential_id,
            input_tokens,
            output_tokens,
            "透传响应统计落库"
        );
    }
}

impl Drop for PassthroughUsageSniffer {
    fn drop(&mut self) {
        self.finalize();
    }
}

/// 从 Anthropic SSE 尾缓冲提取 usage
///
/// 返回 `(input_tokens, output_tokens, cache_read, cache_write)`。
/// input/cache 来自 `message_start` 的 usage；output 来自最后一次 `message_delta` 的 usage。
fn sniff_anthropic_usage(text: &str) -> Option<(i64, i64, i64, i64)> {
    let mut input = 0i64;
    let mut output = 0i64;
    let mut cache_read = 0i64;
    let mut cache_write = 0i64;
    let mut found = false;

    // 逐个 data: 行解析 JSON，累积 usage（message_delta 的 output_tokens 取最后一次）
    for line in text.lines() {
        let line = line.trim_start();
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // message_start.message.usage
        if let Some(usage) = json
            .get("message")
            .and_then(|m| m.get("usage"))
            .or_else(|| json.get("usage"))
        {
            if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                input = v;
                found = true;
            }
            if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                output = v;
                found = true;
            }
            if let Some(v) = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64())
            {
                cache_read = v;
            }
            if let Some(v) = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_i64())
            {
                cache_write = v;
            }
        }
    }

    if found {
        Some((input, output, cache_read, cache_write))
    } else {
        None
    }
}

/// 从 OpenAI Responses SSE 尾缓冲提取 usage
///
/// 返回 `(input_tokens, output_tokens, 0, 0)`。取 `response.completed` 事件中
/// `response.usage` 的 `input_tokens` / `output_tokens`。
fn sniff_openai_usage(text: &str) -> Option<(i64, i64, i64, i64)> {
    let mut input = 0i64;
    let mut output = 0i64;
    let mut found = false;

    for line in text.lines() {
        let line = line.trim_start();
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // response.completed / response.incomplete 携带 response.usage
        let usage = json
            .get("response")
            .and_then(|r| r.get("usage"))
            .or_else(|| json.get("usage"));
        if let Some(usage) = usage {
            if let Some(v) = usage
                .get("input_tokens")
                .and_then(|v| v.as_i64())
                .or_else(|| usage.get("prompt_tokens").and_then(|v| v.as_i64()))
            {
                input = v;
                found = true;
            }
            if let Some(v) = usage
                .get("output_tokens")
                .and_then(|v| v.as_i64())
                .or_else(|| usage.get("completion_tokens").and_then(|v| v.as_i64()))
            {
                output = v;
                found = true;
            }
        }
    }

    if found {
        Some((input, output, 0, 0))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sniff_anthropic_usage() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":20,\"cache_creation_input_tokens\":5,\"output_tokens\":1}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":250}}\n\n",
        );
        let (i, o, cr, cw) = sniff_anthropic_usage(sse).unwrap();
        assert_eq!(i, 100);
        assert_eq!(o, 250);
        assert_eq!(cr, 20);
        assert_eq!(cw, 5);
    }

    #[test]
    fn test_sniff_anthropic_usage_none() {
        let sse = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
        assert!(sniff_anthropic_usage(sse).is_none());
    }

    #[test]
    fn test_sniff_openai_usage() {
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":300,\"output_tokens\":80}}}\n\n",
        );
        let (i, o, _, _) = sniff_openai_usage(sse).unwrap();
        assert_eq!(i, 300);
        assert_eq!(o, 80);
    }

    #[test]
    fn test_tail_buffer_truncation() {
        let mut sniffer = PassthroughUsageSniffer::new("m".into(), 1, PassthroughKind::Anthropic);
        // 喂入超过上限的数据，确认不 panic 且尾部保留
        let big = Bytes::from(vec![b'x'; TAIL_BUFFER_MAX + 1000]);
        sniffer.feed(&big);
        assert!(sniffer.tail.len() <= TAIL_BUFFER_MAX);
        sniffer.finalized = true; // 避免 Drop 落库
    }
}

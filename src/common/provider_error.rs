//! 上游错误到客户端响应的公共辅助

use std::time::Duration;

use axum::http::{header, HeaderValue};
use axum::response::Response;

/// 在 429 响应中按需写入 `Retry-After` 头（由 `passthroughRetryAfter` 配置控制）。
pub fn insert_retry_after_header(
    resp: &mut Response,
    retry_after: Option<Duration>,
    passthrough: bool,
) {
    if !passthrough {
        return;
    }
    if let Some(ra) = retry_after {
        if let Ok(value) = HeaderValue::from_str(&ra.as_secs().max(1).to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
}

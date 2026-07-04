//! 轻量 Prometheus 文本格式指标（无额外依赖）

use std::sync::atomic::{AtomicU64, Ordering};

/// 进程级指标计数器
pub struct Metrics {
    pub requests_success: AtomicU64,
    pub requests_error: AtomicU64,
    pub local_rpm_rejected: AtomicU64,
    pub stream_decode_failures: AtomicU64,
    pub upstream_rate_limited: AtomicU64,
}

impl Metrics {
    pub const fn new() -> Self {
        Self {
            requests_success: AtomicU64::new(0),
            requests_error: AtomicU64::new(0),
            local_rpm_rejected: AtomicU64::new(0),
            stream_decode_failures: AtomicU64::new(0),
            upstream_rate_limited: AtomicU64::new(0),
        }
    }

    pub fn render_prometheus(&self, credentials_available: usize, credentials_total: usize) -> String {
        let success = self.requests_success.load(Ordering::Relaxed);
        let error = self.requests_error.load(Ordering::Relaxed);
        let local_rpm = self.local_rpm_rejected.load(Ordering::Relaxed);
        let decode_fail = self.stream_decode_failures.load(Ordering::Relaxed);
        let upstream_429 = self.upstream_rate_limited.load(Ordering::Relaxed);

        format!(
            concat!(
                "# HELP kiro_requests_success_total 成功完成的上游 API 请求数\n",
                "# TYPE kiro_requests_success_total counter\n",
                "kiro_requests_success_total {success}\n",
                "# HELP kiro_requests_error_total 重试耗尽或不可恢复的上游 API 错误数\n",
                "# TYPE kiro_requests_error_total counter\n",
                "kiro_requests_error_total {error}\n",
                "# HELP kiro_local_rpm_rejected_total 本地凭据 RPM 限流拒绝数（客户端 429）\n",
                "# TYPE kiro_local_rpm_rejected_total counter\n",
                "kiro_local_rpm_rejected_total {local_rpm}\n",
                "# HELP kiro_stream_decode_failures_total 上游 event-stream 解码失败数\n",
                "# TYPE kiro_stream_decode_failures_total counter\n",
                "kiro_stream_decode_failures_total {decode_fail}\n",
                "# HELP kiro_upstream_rate_limited_total 上游返回 429 且重试耗尽次数\n",
                "# TYPE kiro_upstream_rate_limited_total counter\n",
                "kiro_upstream_rate_limited_total {upstream_429}\n",
                "# HELP kiro_credentials_available 当前可用（未禁用）凭据数\n",
                "# TYPE kiro_credentials_available gauge\n",
                "kiro_credentials_available {credentials_available}\n",
                "# HELP kiro_credentials_total 凭据总数\n",
                "# TYPE kiro_credentials_total gauge\n",
                "kiro_credentials_total {credentials_total}\n",
            ),
            success = success,
            error = error,
            local_rpm = local_rpm,
            decode_fail = decode_fail,
            upstream_429 = upstream_429,
            credentials_available = credentials_available,
            credentials_total = credentials_total,
        )
    }
}

pub static METRICS: Metrics = Metrics::new();

pub fn inc_request_success() {
    METRICS.requests_success.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_request_error() {
    METRICS.requests_error.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_local_rpm_rejected() {
    METRICS.local_rpm_rejected.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_stream_decode_failure() {
    METRICS.stream_decode_failures.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_upstream_rate_limited() {
    METRICS.upstream_rate_limited.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_prometheus_contains_counters() {
        inc_request_success();
        let body = METRICS.render_prometheus(2, 3);
        assert!(body.contains("kiro_requests_success_total 1"));
        assert!(body.contains("kiro_credentials_available 2"));
        assert!(body.contains("kiro_credentials_total 3"));
    }
}

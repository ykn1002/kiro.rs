//! 轻量 Prometheus 文本格式指标（无额外依赖）

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

/// 进程启动时刻（首次读取时初始化），用于计算运行时长
static START_INSTANT: OnceLock<Instant> = OnceLock::new();

fn start_instant() -> Instant {
    *START_INSTANT.get_or_init(Instant::now)
}

/// 记录进程启动时刻（在 main 早期调用一次即可，未调用时首次读取会惰性初始化）
pub fn init_start_time() {
    let _ = start_instant();
}

/// 进程级指标计数器
pub struct Metrics {
    pub requests_success: AtomicU64,
    pub requests_error: AtomicU64,
    pub local_rpm_rejected: AtomicU64,
    pub stream_decode_failures: AtomicU64,
    pub upstream_rate_limited: AtomicU64,
    /// 上游响应体在传输中途断开（HTTP 头已 200，body 读取失败）的次数
    pub stream_interrupted: AtomicU64,
    /// 断流后透明重连重放请求的次数
    pub stream_restarted: AtomicU64,
}

/// 指标快照（供 Admin JSON 接口序列化）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSnapshot {
    pub requests_success: u64,
    pub requests_error: u64,
    pub local_rpm_rejected: u64,
    pub stream_decode_failures: u64,
    pub upstream_rate_limited: u64,
    pub stream_interrupted: u64,
    pub stream_restarted: u64,
    /// 进程运行时长（秒）
    pub uptime_seconds: u64,
}

impl Metrics {
    pub const fn new() -> Self {
        Self {
            requests_success: AtomicU64::new(0),
            requests_error: AtomicU64::new(0),
            local_rpm_rejected: AtomicU64::new(0),
            stream_decode_failures: AtomicU64::new(0),
            upstream_rate_limited: AtomicU64::new(0),
            stream_interrupted: AtomicU64::new(0),
            stream_restarted: AtomicU64::new(0),
        }
    }

    /// 读取当前计数器快照
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests_success: self.requests_success.load(Ordering::Relaxed),
            requests_error: self.requests_error.load(Ordering::Relaxed),
            local_rpm_rejected: self.local_rpm_rejected.load(Ordering::Relaxed),
            stream_decode_failures: self.stream_decode_failures.load(Ordering::Relaxed),
            upstream_rate_limited: self.upstream_rate_limited.load(Ordering::Relaxed),
            stream_interrupted: self.stream_interrupted.load(Ordering::Relaxed),
            stream_restarted: self.stream_restarted.load(Ordering::Relaxed),
            uptime_seconds: start_instant().elapsed().as_secs(),
        }
    }

    pub fn render_prometheus(
        &self,
        credentials_available: usize,
        credentials_total: usize,
    ) -> String {
        let success = self.requests_success.load(Ordering::Relaxed);
        let error = self.requests_error.load(Ordering::Relaxed);
        let local_rpm = self.local_rpm_rejected.load(Ordering::Relaxed);
        let decode_fail = self.stream_decode_failures.load(Ordering::Relaxed);
        let upstream_429 = self.upstream_rate_limited.load(Ordering::Relaxed);
        let interrupted = self.stream_interrupted.load(Ordering::Relaxed);
        let restarted = self.stream_restarted.load(Ordering::Relaxed);

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
                "# HELP kiro_stream_interrupted_total 上游响应体传输中途断开次数\n",
                "# TYPE kiro_stream_interrupted_total counter\n",
                "kiro_stream_interrupted_total {interrupted}\n",
                "# HELP kiro_stream_restarted_total 断流后透明重连重放请求次数\n",
                "# TYPE kiro_stream_restarted_total counter\n",
                "kiro_stream_restarted_total {restarted}\n",
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
            interrupted = interrupted,
            restarted = restarted,
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
    METRICS
        .stream_decode_failures
        .fetch_add(1, Ordering::Relaxed);
}

pub fn inc_upstream_rate_limited() {
    METRICS
        .upstream_rate_limited
        .fetch_add(1, Ordering::Relaxed);
}

pub fn inc_stream_interrupted() {
    METRICS.stream_interrupted.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_stream_restarted() {
    METRICS.stream_restarted.fetch_add(1, Ordering::Relaxed);
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

    #[test]
    fn test_snapshot_serializes_camel_case() {
        let snap = METRICS.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("requestsSuccess"));
        assert!(json.contains("uptimeSeconds"));
        assert!(json.contains("streamRestarted"));
    }
}

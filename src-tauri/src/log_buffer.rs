//! 内存日志缓冲 + tracing Layer。
//!
//! 桌面版日志原本只写 stdout（GUI 下无人可见）。这里加一个 tracing Layer，
//! 把格式化后的日志行存入一个有上限的环形缓冲，供前端「日志」Tab 通过 IPC 拉取。
//! 每行带全局递增序号，前端按“上次序号之后”拉增量，避免重复。
//!
//! 捕获开关（[`set_enabled`]）默认开启；关闭后 Layer 不再写入缓冲（stdout 输出不受影响）。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// 缓冲最多保留的日志行数（超出丢弃最旧）。
const MAX_LINES: usize = 2000;

/// 单条日志行。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    /// 全局递增序号，供前端拉增量
    pub seq: u64,
    /// 时间戳（RFC3339，本地时区）
    pub ts: String,
    /// 级别（ERROR/WARN/INFO/DEBUG/TRACE）
    pub level: String,
    /// 目标模块（target）
    pub target: String,
    /// 消息正文（含字段）
    pub message: String,
}

struct Buffer {
    lines: VecDeque<LogLine>,
}

static BUFFER: Mutex<Buffer> = Mutex::new(Buffer {
    lines: VecDeque::new(),
});
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);
static ENABLED: AtomicBool = AtomicBool::new(true);

/// 捕获是否开启。
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 开启/关闭捕获。关闭后新日志不入缓冲；已有缓冲保留。
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// 清空缓冲（序号继续递增，不重置）。
pub fn clear() {
    BUFFER.lock().lines.clear();
}

/// 拉取序号 > `after` 的日志行（按序号升序）。
pub fn since(after: u64) -> Vec<LogLine> {
    BUFFER
        .lock()
        .lines
        .iter()
        .filter(|l| l.seq > after)
        .cloned()
        .collect()
}

fn push(level: &str, target: &str, message: String) {
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let line = LogLine {
        seq,
        ts,
        level: level.to_string(),
        target: target.to_string(),
        message,
    };
    let mut buf = BUFFER.lock();
    if buf.lines.len() >= MAX_LINES {
        buf.lines.pop_front();
    }
    buf.lines.push_back(line);
}

/// 从 event 字段里抽取消息文本（`message` 字段为主，其余字段追加为 k=v）。
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={}", field.name(), value);
        }
    }
}

/// 把日志事件写入内存缓冲的 tracing Layer。
pub struct BufferLayer;

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !is_enabled() {
            return;
        }
        let meta = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = if visitor.fields.is_empty() {
            visitor.message
        } else if visitor.message.is_empty() {
            visitor.fields
        } else {
            format!("{} {}", visitor.message, visitor.fields)
        };
        push(&meta.level().to_string(), meta.target(), message);
    }
}

//! 各模型累计调用统计（调用次数 + 输入/输出 token），全局单例 + 落盘持久化。
//!
//! 与 [`crate::metrics`] 的全局计数器同属进程级统计，但按「模型展示名」分组，
//! 并持久化到 `kiro_model_stats.json`（进程重启后累计值保留）。
//!
//! 记录点在各出口路径的最终收尾处（见 handlers/stream），每请求恰好一次，
//! token 取服务端返回给客户端的最终值（metadataEvent 精确值优先）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 落盘去抖间隔：与 token_manager 的 STATS_SAVE_DEBOUNCE 对齐
const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(30);

/// 当前本地日期（YYYY-MM-DD），用于「今日消耗」按天归零
fn today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 单个模型的累计统计（落盘条目）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelStatEntry {
    /// 累计调用次数
    requests: u64,
    /// 累计输入 token
    input_tokens: u64,
    /// 累计输出 token
    output_tokens: u64,
    /// 累计费用（credits，上游 meteringEvent）
    #[serde(default)]
    credits: f64,
    /// 今日计数所属日期（本地时区 YYYY-MM-DD）；跨天则重置今日字段
    #[serde(default)]
    day: String,
    /// 今日调用次数
    #[serde(default)]
    today_requests: u64,
    /// 今日输入 token
    #[serde(default)]
    today_input_tokens: u64,
    /// 今日输出 token
    #[serde(default)]
    today_output_tokens: u64,
    /// 今日费用（credits）
    #[serde(default)]
    today_credits: f64,
}

impl ModelStatEntry {
    /// 若记录日期不是今天，则先把今日字段清零并更新日期
    fn roll_day(&mut self, today: &str) {
        if self.day != today {
            self.day = today.to_string();
            self.today_requests = 0;
            self.today_input_tokens = 0;
            self.today_output_tokens = 0;
            self.today_credits = 0.0;
        }
    }
}

/// 对外快照（供 Admin JSON 序列化）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatSnapshot {
    /// 模型展示名（归一化后的 displayId）
    pub model: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// 累计费用（credits）
    pub credits: f64,
    /// 今日调用次数
    pub today_requests: u64,
    /// 今日输入 token
    pub today_input_tokens: u64,
    /// 今日输出 token
    pub today_output_tokens: u64,
    /// 今日总 token
    pub today_total_tokens: u64,
    /// 今日费用（credits）
    pub today_credits: f64,
}

/// 全局模型统计存储
pub struct ModelStats {
    inner: Mutex<HashMap<String, ModelStatEntry>>,
    /// 落盘文件路径（None = 不持久化，仅内存）
    path: Mutex<Option<PathBuf>>,
    /// 上次落盘时刻（用于去抖）
    last_save_at: Mutex<Option<Instant>>,
}

impl ModelStats {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            path: Mutex::new(None),
            last_save_at: Mutex::new(None),
        }
    }

    /// 记录一次调用（累加次数、token 与费用），随后去抖落盘。
    /// 负值 token/credits 归 0，避免估算路径偶发负数污染累计。
    pub fn record(&self, model: &str, input_tokens: i64, output_tokens: i64, credits: f64) {
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        let today = today_str();
        let inp = input_tokens.max(0) as u64;
        let out = output_tokens.max(0) as u64;
        let cred = if credits > 0.0 { credits } else { 0.0 };
        {
            let mut map = self.inner.lock();
            let entry = map.entry(model.to_string()).or_default();
            entry.roll_day(&today);
            entry.requests += 1;
            entry.input_tokens += inp;
            entry.output_tokens += out;
            entry.credits += cred;
            entry.today_requests += 1;
            entry.today_input_tokens += inp;
            entry.today_output_tokens += out;
            entry.today_credits += cred;
        }
        self.save_debounced();
    }

    /// 返回按调用次数降序的快照。读取时按当天判断今日字段：
    /// 若条目记录的日期不是今天，则今日各项视为 0（跨天自然归零，无需写操作）。
    pub fn snapshot(&self) -> Vec<ModelStatSnapshot> {
        let today = today_str();
        let map = self.inner.lock();
        let mut list: Vec<ModelStatSnapshot> = map
            .iter()
            .map(|(model, e)| {
                let is_today = e.day == today;
                let (tr, ti, to, tc) = if is_today {
                    (
                        e.today_requests,
                        e.today_input_tokens,
                        e.today_output_tokens,
                        e.today_credits,
                    )
                } else {
                    (0, 0, 0, 0.0)
                };
                ModelStatSnapshot {
                    model: model.clone(),
                    requests: e.requests,
                    input_tokens: e.input_tokens,
                    output_tokens: e.output_tokens,
                    total_tokens: e.input_tokens + e.output_tokens,
                    credits: e.credits,
                    today_requests: tr,
                    today_input_tokens: ti,
                    today_output_tokens: to,
                    today_total_tokens: ti + to,
                    today_credits: tc,
                }
            })
            .collect();
        list.sort_by(|a, b| {
            b.requests
                .cmp(&a.requests)
                .then_with(|| a.model.cmp(&b.model))
        });
        list
    }

    /// 设置落盘路径并从磁盘加载既有累计值（启动时调用一次）
    pub fn init_path(&self, path: Option<PathBuf>) {
        if let Some(ref p) = path
            && let Ok(content) = std::fs::read_to_string(p)
        {
            match serde_json::from_str::<HashMap<String, ModelStatEntry>>(&content) {
                Ok(loaded) => *self.inner.lock() = loaded,
                Err(e) => tracing::warn!("解析模型统计失败，将忽略: {}", e),
            }
        }
        *self.path.lock() = path;
        *self.last_save_at.lock() = Some(Instant::now());
    }

    /// 去抖落盘：距上次落盘超过 SAVE_DEBOUNCE 才真正写
    fn save_debounced(&self) {
        let should_flush = {
            let last = *self.last_save_at.lock();
            match last {
                Some(t) => t.elapsed() >= SAVE_DEBOUNCE,
                None => true,
            }
        };
        if should_flush {
            self.save();
        }
    }

    /// 立即落盘（进程退出或去抖触发时）
    pub fn save(&self) {
        let path = match self.path.lock().clone() {
            Some(p) => p,
            None => return,
        };
        let json = {
            let map = self.inner.lock();
            match serde_json::to_string_pretty(&*map) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("序列化模型统计失败: {}", e);
                    return;
                }
            }
        };
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!("写入模型统计失败 {:?}: {}", path, e);
            return;
        }
        *self.last_save_at.lock() = Some(Instant::now());
    }
}

static MODEL_STATS: OnceLock<ModelStats> = OnceLock::new();

/// 全局单例访问（首次访问惰性初始化）
pub fn global() -> &'static ModelStats {
    MODEL_STATS.get_or_init(ModelStats::new)
}

/// 便捷记录入口
pub fn record(model: &str, input_tokens: i64, output_tokens: i64, credits: f64) {
    global().record(model, input_tokens, output_tokens, credits);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_snapshot() {
        let stats = ModelStats::new();
        stats.record("claude-opus-4-8", 100, 50, 0.25);
        stats.record("claude-opus-4-8", 200, 30, 0.75);
        stats.record("claude-sonnet-4-6", 10, 5, 0.1);

        let snap = stats.snapshot();
        // opus 调用 2 次应排在前
        assert_eq!(snap[0].model, "claude-opus-4-8");
        assert_eq!(snap[0].requests, 2);
        assert_eq!(snap[0].input_tokens, 300);
        assert_eq!(snap[0].output_tokens, 80);
        assert_eq!(snap[0].total_tokens, 380);
        assert!((snap[0].credits - 1.0).abs() < 1e-9);
        // 刚记录的都算今日
        assert_eq!(snap[0].today_requests, 2);
        assert_eq!(snap[0].today_total_tokens, 380);
        assert!((snap[0].today_credits - 1.0).abs() < 1e-9);
        assert_eq!(snap[1].model, "claude-sonnet-4-6");
        assert_eq!(snap[1].requests, 1);
        assert_eq!(snap[1].today_requests, 1);
    }

    #[test]
    fn test_stale_day_zeroes_today() {
        let stats = ModelStats::new();
        stats.record("m", 100, 50, 1.0);
        // 手动把记录日期改成昨天，模拟跨天
        {
            let mut map = stats.inner.lock();
            map.get_mut("m").unwrap().day = "2000-01-01".to_string();
        }
        let snap = stats.snapshot();
        // 累计保留，今日归零
        assert_eq!(snap[0].requests, 1);
        assert_eq!(snap[0].total_tokens, 150);
        assert!((snap[0].credits - 1.0).abs() < 1e-9, "累计费用应保留");
        assert_eq!(snap[0].today_requests, 0);
        assert_eq!(snap[0].today_total_tokens, 0);
        assert_eq!(snap[0].today_credits, 0.0, "今日费用跨天应归零");
    }

    #[test]
    fn test_negative_tokens_clamped() {
        let stats = ModelStats::new();
        stats.record("m", -5, -3, -2.0);
        let snap = stats.snapshot();
        assert_eq!(snap[0].input_tokens, 0);
        assert_eq!(snap[0].output_tokens, 0);
        assert_eq!(snap[0].requests, 1);
        assert_eq!(snap[0].credits, 0.0, "负费用应归零");
    }

    #[test]
    fn test_empty_model_ignored() {
        let stats = ModelStats::new();
        stats.record("  ", 10, 10, 1.0);
        assert!(stats.snapshot().is_empty());
    }
}

//! 监控时间序列存储（SQLite 预聚合小时桶）。
//!
//! 与 [`crate::model_stats`]（内存累计 + JSON 快照，供实时监控/RPM 表）互补：
//! 本模块把每次请求的用量按「小时桶 × 模型 × 上游凭据」预聚合落盘到 SQLite，
//! 支撑 Admin UI 的时间范围趋势图与按模型/凭据分布。
//!
//! # 设计
//!
//! - `rusqlite::Connection` 非 async 友好，故采用 **actor 模式**：独立 OS 线程
//!   持有连接，通过 channel 接收记录/查询消息。请求路径调用 [`record`] 只是往
//!   channel 发一条消息，零磁盘 IO（符合「瓶颈是 IO-bound，别在请求路径加阻塞」）。
//! - 写入在内存 `pending` 里累加增量，每 [`FLUSH_INTERVAL`] 批量 UPSERT 一次。
//! - 保留窗口 [`RETENTION_DAYS`] 天，flush 时顺带清理过期桶。
//! - 无落盘路径（仅内存模式）时整体降级为空操作，查询返回空。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use rusqlite::Connection;

/// 批量落盘间隔
const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

/// 数据保留天数（与 UI 最大时间范围 30 天对齐）
const RETENTION_DAYS: i64 = 30;

/// 记录 channel 容量：满时丢弃（监控数据非关键，不应阻塞请求）
const RECORD_CHANNEL_CAP: usize = 4096;

/// 一次用量记录（发往 actor 线程）
#[derive(Debug, Clone)]
struct RecordMsg {
    /// Unix 秒（UTC）
    ts: i64,
    /// 归一化模型展示名
    model: String,
    /// 上游凭据 id（-1 = 未知）
    credential_id: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    /// 上游计费 credits 消耗（meteringEvent，可为分数）
    credits: f64,
}

/// actor 消息
enum Msg {
    Record(RecordMsg),
    Query {
        from: i64,
        to: i64,
        bucket: Bucket,
        reply: Sender<TimeseriesData>,
    },
    /// 立即落盘（测试/退出用）
    Flush(Sender<()>),
}

/// 聚合粒度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Hour,
    Day,
}

impl Bucket {
    /// 桶宽（秒）
    fn width(self) -> i64 {
        match self {
            Bucket::Hour => 3600,
            Bucket::Day => 86400,
        }
    }

    pub fn parse(s: &str) -> Bucket {
        match s {
            "day" => Bucket::Day,
            _ => Bucket::Hour,
        }
    }
}

/// 单个时间桶的聚合值
#[derive(Debug, Clone, Default)]
pub struct TimePoint {
    pub bucket: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 计费 credits 消耗合计
    pub credits: f64,
}

/// 某个维度（模型 / 凭据）的区间聚合值
#[derive(Debug, Clone, Default)]
pub struct DimPoint {
    /// 模型名，或凭据 id 的字符串形式
    pub key: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 计费 credits 消耗合计
    pub credits: f64,
}

/// 时间序列查询结果
#[derive(Debug, Clone, Default)]
pub struct TimeseriesData {
    pub series: Vec<TimePoint>,
    pub by_model: Vec<DimPoint>,
    pub by_credential: Vec<DimPoint>,
}

/// 全局句柄
pub struct StatsDb {
    /// 记录通道；None = 未启用（仅内存模式）
    tx: Option<SyncSender<Msg>>,
}

impl StatsDb {
    /// 未启用（无落盘路径）的空句柄
    fn disabled() -> Self {
        Self { tx: None }
    }

    /// 以指定路径启动 actor 线程
    fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = sync_channel::<Msg>(RECORD_CHANNEL_CAP);
        let builder = std::thread::Builder::new().name("stats-db".into());
        let spawned = builder.spawn(move || actor_loop(path, rx));
        match spawned {
            Ok(_) => Self { tx: Some(tx) },
            Err(e) => {
                tracing::warn!("启动 stats-db 线程失败，监控时间序列不可用: {}", e);
                Self::disabled()
            }
        }
    }

    /// 记录一次用量（非阻塞；通道满或未启用时静默丢弃）
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        model: &str,
        credential_id: i64,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
        credits: f64,
    ) {
        let tx = match &self.tx {
            Some(t) => t,
            None => return,
        };
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        let msg = RecordMsg {
            ts: chrono::Utc::now().timestamp(),
            model: model.to_string(),
            credential_id,
            input_tokens: input_tokens.max(0),
            output_tokens: output_tokens.max(0),
            cache_read_tokens: cache_read_tokens.max(0),
            cache_write_tokens: cache_write_tokens.max(0),
            credits: if credits > 0.0 { credits } else { 0.0 },
        };
        // try_send：通道满则丢弃，绝不阻塞请求路径
        let _ = tx.try_send(Msg::Record(msg));
    }

    /// 查询时间序列（阻塞等待 actor 回复；未启用时返回空）
    pub fn query(&self, from: i64, to: i64, bucket: Bucket) -> TimeseriesData {
        let tx = match &self.tx {
            Some(t) => t,
            None => return TimeseriesData::default(),
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if tx
            .send(Msg::Query {
                from,
                to,
                bucket,
                reply: reply_tx,
            })
            .is_err()
        {
            return TimeseriesData::default();
        }
        reply_rx.recv().unwrap_or_default()
    }

    /// 立即落盘并等待完成（主要供测试/退出）
    pub fn flush(&self) {
        let tx = match &self.tx {
            Some(t) => t,
            None => return,
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if tx.send(Msg::Flush(reply_tx)).is_ok() {
            let _ = reply_rx.recv();
        }
    }
}

/// actor 主循环：持有 Connection，聚合写入 + 响应查询
fn actor_loop(path: PathBuf, rx: std::sync::mpsc::Receiver<Msg>) {
    let mut conn = match open_and_init(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("打开监控数据库失败 {:?}: {}", path, e);
            // 仍然把通道抽干，避免发送端在满通道上偶发阻塞
            while rx.recv().is_ok() {}
            return;
        }
    };

    // 启动时清理一次过期数据
    if let Err(e) = purge_old(&conn) {
        tracing::warn!("清理过期监控数据失败: {}", e);
    }

    // 内存待写增量：(hour_bucket, model, credential_id) -> 累加值
    let mut pending: HashMap<(i64, String, i64), TimePoint> = HashMap::new();
    let mut last_flush = Instant::now();

    loop {
        // 用带超时的 recv，保证即便无新消息也能定期 flush
        let timeout = FLUSH_INTERVAL
            .checked_sub(last_flush.elapsed())
            .unwrap_or(Duration::ZERO);
        match rx.recv_timeout(timeout) {
            Ok(Msg::Record(r)) => {
                let hour = (r.ts / 3600) * 3600;
                let e = pending.entry((hour, r.model, r.credential_id)).or_default();
                e.bucket = hour;
                e.requests += 1;
                e.input_tokens += r.input_tokens;
                e.output_tokens += r.output_tokens;
                e.cache_read_tokens += r.cache_read_tokens;
                e.cache_write_tokens += r.cache_write_tokens;
                e.credits += r.credits;
            }
            Ok(Msg::Query {
                from,
                to,
                bucket,
                reply,
            }) => {
                // 先 flush 待写增量，保证查询看到最新数据
                flush_pending(&mut conn, &mut pending);
                last_flush = Instant::now();
                let data = query_db(&conn, from, to, bucket).unwrap_or_default();
                let _ = reply.send(data);
            }
            Ok(Msg::Flush(reply)) => {
                flush_pending(&mut conn, &mut pending);
                let _ = purge_old(&conn);
                last_flush = Instant::now();
                let _ = reply.send(());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                flush_pending(&mut conn, &mut pending);
                let _ = purge_old(&conn);
                last_flush = Instant::now();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // 所有发送端已释放：最后 flush 一次后退出
                flush_pending(&mut conn, &mut pending);
                return;
            }
        }
    }
}

/// 打开数据库并建表
fn open_and_init(path: &PathBuf) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_hourly (
            hour_bucket        INTEGER NOT NULL,
            model              TEXT    NOT NULL,
            credential_id      INTEGER NOT NULL DEFAULT -1,
            requests           INTEGER NOT NULL DEFAULT 0,
            input_tokens       INTEGER NOT NULL DEFAULT 0,
            output_tokens      INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
            cache_write_tokens INTEGER NOT NULL DEFAULT 0,
            credits            REAL    NOT NULL DEFAULT 0,
            PRIMARY KEY (hour_bucket, model, credential_id)
        );
        CREATE INDEX IF NOT EXISTS idx_usage_hour ON usage_hourly(hour_bucket);",
    )?;
    // 旧库（无 credits 列）迁移：ADD COLUMN 幂等失败即忽略（列已存在）
    let _ = conn.execute(
        "ALTER TABLE usage_hourly ADD COLUMN credits REAL NOT NULL DEFAULT 0",
        [],
    );
    Ok(conn)
}

/// 批量把内存增量 UPSERT 累加进库
fn flush_pending(conn: &mut Connection, pending: &mut HashMap<(i64, String, i64), TimePoint>) {
    if pending.is_empty() {
        return;
    }
    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("开启监控写入事务失败: {}", e);
            return;
        }
    };
    {
        let mut stmt = match tx.prepare_cached(
            "INSERT INTO usage_hourly
                (hour_bucket, model, credential_id, requests, input_tokens,
                 output_tokens, cache_read_tokens, cache_write_tokens, credits)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(hour_bucket, model, credential_id) DO UPDATE SET
                requests = requests + excluded.requests,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens,
                credits = credits + excluded.credits",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("准备监控写入语句失败: {}", e);
                return;
            }
        };
        for ((hour, model, cred), v) in pending.iter() {
            if let Err(e) = stmt.execute(rusqlite::params![
                hour,
                model,
                cred,
                v.requests,
                v.input_tokens,
                v.output_tokens,
                v.cache_read_tokens,
                v.cache_write_tokens,
                v.credits,
            ]) {
                tracing::warn!("写入监控数据失败: {}", e);
            }
        }
    }
    if let Err(e) = tx.commit() {
        tracing::warn!("提交监控写入事务失败: {}", e);
        return;
    }
    pending.clear();
}

/// 删除保留窗口外的桶
fn purge_old(conn: &Connection) -> rusqlite::Result<()> {
    let cutoff = chrono::Utc::now().timestamp() - RETENTION_DAYS * 86400;
    conn.execute(
        "DELETE FROM usage_hourly WHERE hour_bucket < ?1",
        rusqlite::params![cutoff],
    )?;
    Ok(())
}

/// 在 `[from, to)` 区间内按 bucket 聚合，同时给出按模型/凭据分布
fn query_db(
    conn: &Connection,
    from: i64,
    to: i64,
    bucket: Bucket,
) -> rusqlite::Result<TimeseriesData> {
    let width = bucket.width();

    // 时间序列：按桶归并
    let mut series_stmt = conn.prepare_cached(
        "SELECT (hour_bucket / ?1) * ?1 AS b,
                SUM(requests), SUM(input_tokens), SUM(output_tokens),
                SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(credits)
         FROM usage_hourly
         WHERE hour_bucket >= ?2 AND hour_bucket < ?3
         GROUP BY b ORDER BY b",
    )?;
    let series = series_stmt
        .query_map(rusqlite::params![width, from, to], |row| {
            Ok(TimePoint {
                bucket: row.get(0)?,
                requests: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cache_read_tokens: row.get(4)?,
                cache_write_tokens: row.get(5)?,
                credits: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // 按模型分布
    let mut model_stmt = conn.prepare_cached(
        "SELECT model,
                SUM(requests), SUM(input_tokens), SUM(output_tokens),
                SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(credits)
         FROM usage_hourly
         WHERE hour_bucket >= ?1 AND hour_bucket < ?2
         GROUP BY model ORDER BY SUM(requests) DESC",
    )?;
    let by_model = model_stmt
        .query_map(rusqlite::params![from, to], |row| {
            Ok(DimPoint {
                key: row.get(0)?,
                requests: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cache_read_tokens: row.get(4)?,
                cache_write_tokens: row.get(5)?,
                credits: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // 按凭据分布
    let mut cred_stmt = conn.prepare_cached(
        "SELECT credential_id,
                SUM(requests), SUM(input_tokens), SUM(output_tokens),
                SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(credits)
         FROM usage_hourly
         WHERE hour_bucket >= ?1 AND hour_bucket < ?2
         GROUP BY credential_id ORDER BY SUM(requests) DESC",
    )?;
    let by_credential = cred_stmt
        .query_map(rusqlite::params![from, to], |row| {
            let cred: i64 = row.get(0)?;
            Ok(DimPoint {
                key: cred.to_string(),
                requests: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cache_read_tokens: row.get(4)?,
                cache_write_tokens: row.get(5)?,
                credits: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(TimeseriesData {
        series,
        by_model,
        by_credential,
    })
}

static STATS_DB: OnceLock<StatsDb> = OnceLock::new();

/// 初始化全局单例（启动时调用一次）。`None` = 仅内存模式，监控时间序列不落盘。
pub fn init(path: Option<PathBuf>) {
    let db = match path {
        Some(p) => StatsDb::spawn(p),
        None => StatsDb::disabled(),
    };
    if STATS_DB.set(db).is_err() {
        tracing::warn!("stats_db 已初始化，忽略重复 init");
    }
}

/// 全局单例访问（未 init 时返回未启用句柄）
pub fn global() -> &'static StatsDb {
    STATS_DB.get_or_init(StatsDb::disabled)
}

/// 便捷记录入口
#[allow(clippy::too_many_arguments)]
pub fn record(
    model: &str,
    credential_id: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    credits: f64,
) {
    global().record(
        model,
        credential_id,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        credits,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用临时库：随机文件名，测试结束自动删除（含 WAL/SHM 附属文件）
    struct TempDb {
        db: StatsDb,
        path: PathBuf,
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
        }
    }

    fn temp_db() -> TempDb {
        let path =
            std::env::temp_dir().join(format!("kiro-stats-test-{}.db", uuid::Uuid::new_v4()));
        TempDb {
            db: StatsDb::spawn(path.clone()),
            path,
        }
    }

    #[test]
    fn test_record_and_query_hour() {
        let t = temp_db();
        let db = &t.db;
        db.record("claude-opus-4-8", 1, 100, 50, 10, 5, 0.25);
        db.record("claude-opus-4-8", 1, 200, 30, 0, 0, 0.75);
        db.record("claude-sonnet-4-6", 2, 10, 5, 0, 0, 0.1);
        db.flush();

        let now = chrono::Utc::now().timestamp();
        let data = db.query(now - 7200, now + 3600, Bucket::Hour);

        // 序列聚合（同一小时桶内累加）
        let total_req: i64 = data.series.iter().map(|p| p.requests).sum();
        assert_eq!(total_req, 3);
        let total_in: i64 = data.series.iter().map(|p| p.input_tokens).sum();
        assert_eq!(total_in, 310);
        let total_credits: f64 = data.series.iter().map(|p| p.credits).sum();
        assert!((total_credits - 1.1).abs() < 1e-9, "credits 合计应为 1.1");

        // 按模型分布：opus 2 次排在前
        assert_eq!(data.by_model[0].key, "claude-opus-4-8");
        assert_eq!(data.by_model[0].requests, 2);
        assert_eq!(data.by_model[0].input_tokens, 300);
        assert_eq!(data.by_model[0].cache_read_tokens, 10);
        assert!(
            (data.by_model[0].credits - 1.0).abs() < 1e-9,
            "opus credits 应为 1.0"
        );

        // 按凭据分布
        assert_eq!(data.by_credential.len(), 2);
        let cred1 = data.by_credential.iter().find(|d| d.key == "1").unwrap();
        assert_eq!(cred1.requests, 2);
    }

    #[test]
    fn test_day_bucket_aggregates_hours() {
        let t = temp_db();
        let db = &t.db;
        // 同一天不同小时
        db.record("m", -1, 100, 0, 0, 0, 0.0);
        db.flush();
        let now = chrono::Utc::now().timestamp();
        let data = db.query(now - 86400, now + 86400, Bucket::Day);
        // 至少落在一个天桶里
        let total: i64 = data.series.iter().map(|p| p.input_tokens).sum();
        assert_eq!(total, 100);
        // 天桶对齐到 86400
        assert!(data.series.iter().all(|p| p.bucket % 86400 == 0));
    }

    #[test]
    fn test_empty_model_ignored() {
        let t = temp_db();
        let db = &t.db;
        db.record("  ", -1, 10, 10, 0, 0, 1.0);
        db.flush();
        let now = chrono::Utc::now().timestamp();
        let data = db.query(now - 3600, now + 3600, Bucket::Hour);
        assert!(data.series.is_empty());
    }

    #[test]
    fn test_disabled_is_noop() {
        let db = StatsDb::disabled();
        db.record("m", 1, 10, 10, 0, 0, 1.0);
        let data = db.query(0, i64::MAX, Bucket::Hour);
        assert!(data.series.is_empty());
    }

    #[test]
    fn test_negative_clamped() {
        let t = temp_db();
        let db = &t.db;
        db.record("m", 1, -5, -3, -1, -1, -2.0);
        db.flush();
        let now = chrono::Utc::now().timestamp();
        let data = db.query(now - 3600, now + 3600, Bucket::Hour);
        assert_eq!(data.series[0].input_tokens, 0);
        assert_eq!(data.series[0].requests, 1);
        assert_eq!(data.series[0].credits, 0.0, "负 credits 应归零");
    }
}
